//! DLM stage **S5** — the read-only coherent mount (pre-RC engineering
//! spec §6.8 items 1, 4, 5, 6; §6.4's verified refusal; §6.9 S5).
//!
//! §6.4 verified that a read-only second mount was **not possible**:
//! `KvMetaBackend::open` took `flock(LOCK_EX | LOCK_NB)` unconditionally,
//! *before* any read/write classification, and `read_only` derived solely
//! from `sb.unknown_ro() != 0` — a forward-compatibility degradation, not
//! a mount option. There was no `-o ro`, no `--read-only`, no env knob,
//! and cross-host a would-be reader was refused `FreshForeign` by the D0
//! gate.
//!
//! These tests define the contract of the reader mount, and — just as
//! importantly — pin that the reader is **orthogonal to D0**: the
//! single-writer *write* exclusion is not weakened (a second writer is
//! still refused) and not taxed (a reader never refuses a writer, in
//! either mount order).
//!
//! Contracts pinned here (red-first):
//!
//! **Item 1 — a real read-only mount mode**
//! - `open_read_only` mounts a volume a live writer holds (the §6.4
//!   refusal is bypassed for RO: no Layer-A `LOCK_EX`, no claim gate, so
//!   no `FreshForeign`).
//! - A write mount is refused by NOTHING a reader did: reader-then-writer
//!   and writer-then-reader both succeed.
//! - A **second writer** is still refused while a writer holds, readers
//!   present or not (D0 unweakened).
//! - An RO mount session writes **zero bytes** to the volume: no
//!   superblock repair, no `writer_claim`, no checkpoint, no journal.
//! - Every metadata mutation refuses loud, naming the read-only mount.
//! - The guarantee class is its own row: `writer_guard_mode() ==
//!   "reader"`.
//!
//! **Item 6 — reader-side data-plane lockdown** (the write gate extended
//! past metadata): block allocation, `begin_free`, the W1 sole-owner
//! patch, the in-place-overwrite lever, the reclaim queue, and
//! `recover_active_blocks_v3`'s free-completing arm all refuse under the
//! latch.
//!
//! **Item 4 — TTL alignment**: kernel TTLs and the daemon dentry/attr
//! caches derive from the checkpoint cadence (never a hardcoded value);
//! explicit env / `-o` values still win verbatim (the env-knob
//! precedence law), and the writeback cache is off for readers.
//!
//! **Item 5 — purge on revalidation**: the revalidation epoch trigger
//! routes through `TieredCache::purge_block_key` — the ONE call covering
//! all five block-key stores.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::block_reclaim::{ReclaimEntry, ReclaimQueue};
use squeezefs::cache::TieredCache;
use squeezefs::fuse_client::{
    read_only_from_options, read_only_mount, reader_daemon_cache_ttl, set_read_only_mount,
    KernelCacheTtls,
};
use squeezefs::meta_backend::kv::backend::{KvMetaBackend, WriterClaim, WRITER_CLAIM_XATTR};
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::Metadata;
use squeezefs::nvme_dev::NvmeBlockDev;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 64 * 1024 * 1024;

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: Some(1024 * 1024),
        force: false,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn fresh_volume() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(VOL_LEN).unwrap();
    format_v3(f.path(), VOL_LEN, &opts()).await.unwrap();
    f
}

/// The latch is process-global (the `WRITE_VERIFICATION` convention — one
/// mount per daemon process), so every test that arms it restores the
/// default on the way out, including on an assertion unwind. Tests run
/// under the repo's serial gate (`--test-threads=1`).
struct RoLatch;

impl RoLatch {
    fn arm() -> Self {
        set_read_only_mount(true);
        Self
    }
}

impl Drop for RoLatch {
    fn drop(&mut self) {
        set_read_only_mount(false);
    }
}

fn device_digest(path: &std::path::Path) -> u64 {
    let bytes = std::fs::read(path).expect("read volume image");
    xxhash_rust::xxh3::xxh3_64(&bytes)
}

/// A minimal data plane (tier cache + one allocator + one device) — the
/// shape every in-process suite here builds.
async fn data_router(vol_id: &str) -> (squeezefs::routing::DataRouter, NamedTempFile) {
    let b = NamedTempFile::new().unwrap();
    b.as_file().set_len(16 * 1024 * 1024).unwrap();
    let dev = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(vol_id).await.unwrap());
    let cache = TieredCache::new(
        Vec::new(),
        Some("8MB"),
        Some("8MB"),
        Some("8MB"),
        Some("8MB"),
        ba.clone(),
        dev.clone(),
        None,
    )
    .await
    .unwrap();
    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    (squeezefs::routing::DataRouter::new(dlm, cache, ba, dev), b)
}

// ===========================================================================
// Item 1 — the read-only mount mode
// ===========================================================================

/// §6.4's refusal, bypassed for a reader: the volume a live writer holds
/// mounts read-only, and the writer keeps serving across it. This is the
/// whole point of S5 — "1 writer + N readers" needs no lock manager
/// because the reader takes no lease and no claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_mount_admitted_while_a_writer_holds_the_volume() {
    let vol = fresh_volume().await;
    let writer = KvMetaBackend::open(vol.path()).await.expect("write mount");

    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("a read-only mount must be admitted while a writer holds the volume (§6.8 item 1)");
    assert!(reader.is_read_only(), "the reader must latch read-only");

    // The writer is undisturbed.
    Metadata::create(writer.as_ref(), 1, "after", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("the writer keeps serving with a reader attached");

    drop(reader);
    writer.shutdown().await.unwrap();
}

/// D0 orthogonality, direction 1: a reader must never refuse a writer.
/// A retained `flock(LOCK_SH)` would do exactly that (LOCK_SH conflicts
/// with LOCK_EX), which is why the reader's shared probe is not retained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_never_refuses_a_writer_mount() {
    let vol = fresh_volume().await;
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");

    let writer = KvMetaBackend::open(vol.path()).await.expect(
        "a write mount must NOT be refused because a reader is attached (D0 orthogonality)",
    );
    Metadata::create(writer.as_ref(), 1, "w", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("the writer serves normally");

    drop(reader);
    writer.shutdown().await.unwrap();
}

/// D0 unweakened: with readers attached, a SECOND writer is still refused
/// loud, naming the single-writer guard. The reader is orthogonal to the
/// exclusion, never a hole in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_writer_still_refused_with_readers_attached() {
    let vol = fresh_volume().await;
    let writer = KvMetaBackend::open(vol.path()).await.expect("write mount");
    let r1 = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("reader 1");
    let r2 = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("reader 2 — N readers coexist");

    let err = KvMetaBackend::open(vol.path())
        .await
        .expect_err("a second WRITER must still be refused (D0)")
        .to_string();
    assert!(
        err.contains("single-writer") || err.contains("writer lock"),
        "the refusal must still name the single-writer guard: {err}"
    );

    drop((r1, r2));
    writer.shutdown().await.unwrap();
}

/// A reader mutates nothing. Not the superblock (the DUR-5 self-heal
/// rewrite is a write-mount-only pass), not the claim, not the journal,
/// not a checkpoint: the device image is byte-identical across an RO
/// session that also attempts a mutation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_mount_writes_nothing_to_the_volume() {
    let vol = fresh_volume().await;
    // Settle the image: one write mount, cleanly shut down.
    KvMetaBackend::open(vol.path())
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    let before = device_digest(vol.path());

    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");
    let _ = Metadata::create(reader.as_ref(), 1, "nope", libc::S_IFREG | 0o644, 0, 0).await;
    let _ = reader.sync_device().await;
    drop(reader);

    assert_eq!(
        before,
        device_digest(vol.path()),
        "a read-only mount session must not write ONE byte to the volume"
    );
}

/// The metadata write gate: every mutation refuses loud and names the
/// read-only mount (not the §4.11 unknown-ro-bits degradation, which is a
/// different cause and must stay distinguishable in the message).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_mount_refuses_metadata_mutations_loud() {
    let vol = fresh_volume().await;
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");

    let err = Metadata::create(reader.as_ref(), 1, "x", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect_err("create must refuse on a read-only mount")
        .to_string();
    assert!(
        err.contains("read-only"),
        "the refusal must name the read-only mount: {err}"
    );
    assert!(
        err.contains("-o ro") || err.contains("--read-only"),
        "the refusal must name the operator surface that produced it: {err}"
    );

    // A reader can still READ.
    Metadata::getattr(reader.as_ref(), 1)
        .await
        .expect("the root inode still reads");
}

/// The guarantee class is its own row on the stats surface: a reader is
/// neither `flock+pr` nor `flock+claim` nor `unguarded` — it is `reader`
/// (no Layer-A lock retained, no claim written, no PR registration).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_mount_reports_the_reader_guarantee_class() {
    let vol = fresh_volume().await;
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");
    assert_eq!(
        reader.writer_guard_mode(),
        "reader",
        "the reader mount needs its own guarantee-class row"
    );
}

/// A reader writes no `writer_claim` — so it can never be mistaken for a
/// writer by the D0 ladder, by `squeezefs clients`, or by the `format`
/// preflight, and a foreign writer's claim survives an RO session
/// untouched (no reclaim, no preempt, no heartbeat).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_mount_leaves_a_foreign_writer_claim_untouched() {
    let vol = fresh_volume().await;
    // Plant a fresh FOREIGN claim (another host's boot id): a write mount
    // would refuse `FreshForeign` here; the reader must not care.
    {
        let be = KvMetaBackend::open(vol.path()).await.unwrap();
        let claim = WriterClaim {
            id: "foreign-writer".to_string(),
            pid: 999_999,
            boot: "00000000-0000-0000-0000-000000000000".to_string(),
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            term: 0,
        };
        be.setxattr_internal(1, WRITER_CLAIM_XATTR, &claim.encode())
            .await
            .unwrap();
        be.sync_device().await.unwrap();
        drop(be);
    }

    // The write mount refuses (D0 unchanged) …
    let werr = KvMetaBackend::open(vol.path())
        .await
        .expect_err("a fresh foreign claim still refuses a WRITE mount")
        .to_string();
    assert!(werr.contains("claimed by a live writer") || werr.contains("single-writer"));

    // … and the reader is admitted, leaving the claim exactly as found.
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("the reader bypasses the FreshForeign refusal (§6.8 item 1)");
    let claim = reader
        .read_writer_claim()
        .await
        .expect("the foreign claim is still there");
    assert_eq!(claim.id, "foreign-writer", "the reader must not touch it");
    assert_eq!(claim.pid, 999_999);
}

// ===========================================================================
// Item 6 — reader-side data-plane lockdown
// ===========================================================================

/// The write gate extended past metadata (§6.8 item 1's list): block
/// allocation refuses under the latch. Without this a reader's allocator
/// — seeded from ITS view of the tree — would hand out offsets a live
/// writer owns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_latch_refuses_block_allocation() {
    let ba = BlockAllocator::new("ro_alloc_test").await.unwrap();
    ba.set_capacity_bytes(1024 * 1024 * 1024);
    // Writable: the allocation succeeds (the control).
    let off = ba.allocate_block().await.expect("writable allocation");
    ba.finish_free(off);

    let _latch = RoLatch::arm();
    let err = ba
        .allocate_block()
        .await
        .expect_err("allocation must refuse on a read-only mount")
        .to_string();
    assert!(
        err.contains("read-only"),
        "the refusal must name the read-only mount: {err}"
    );
    assert!(
        ba.allocate_block_below(64).is_none(),
        "contiguity picks too"
    );
    assert!(
        ba.allocate_block_at_or_above(0).is_err(),
        "the at-or-above pick too"
    );
    assert!(
        ba.allocate_specific_block(7).await.is_err(),
        "the specific pick too"
    );
}

/// `begin_free` is the terminal-free entry every reclaim rides. A reader
/// that free-completes an offset would make a live writer's block
/// reallocatable — §6.3's reclaim/discard hazard, exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_latch_refuses_terminal_frees() {
    let ba = BlockAllocator::new("ro_free_test").await.unwrap();
    ba.set_capacity_bytes(1024 * 1024 * 1024);
    let off = ba.allocate_block().await.expect("writable allocation");

    let _latch = RoLatch::arm();
    assert!(
        !ba.begin_free(off),
        "a reader must never complete a free — the offset stays referenced"
    );
    assert!(
        ba.free_block(off).await.is_err(),
        "the whole terminal-free path refuses"
    );
}

/// W1's sole-owner patch predicate: `begin_patch_sole_owner` reads a
/// PROCESS-LOCAL refcount map (§6.3), so on a reader it can prove nothing
/// about a block a writer may have cloned. It refuses outright — and the
/// reader never writes anyway, which is why this needs no new
/// ineligibility counter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_latch_refuses_the_w1_sole_owner_patch() {
    let ba = BlockAllocator::new("ro_patch_test").await.unwrap();
    ba.set_capacity_bytes(1024 * 1024 * 1024);
    let off = ba.allocate_block().await.expect("writable allocation");
    assert!(
        ba.begin_patch_sole_owner(off),
        "control: a sole-owned block is patchable on a write mount"
    );
    ba.publish_block(off);

    let _latch = RoLatch::arm();
    assert!(
        !ba.begin_patch_sole_owner(off),
        "the W1 in-place patch must refuse on a read-only mount"
    );
}

/// The in-place-overwrite lever is substrate-measured and opt-in; on a
/// reader it is off regardless of the knob (the latch wins over the env).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_latch_disables_the_inplace_overwrite_lever() {
    squeezefs::fuse_client::set_inplace_overwrite(true);
    assert!(
        squeezefs::fuse_client::inplace_overwrite_enabled(),
        "control: the lever is armed"
    );
    {
        let _latch = RoLatch::arm();
        assert!(
            !squeezefs::fuse_client::inplace_overwrite_enabled(),
            "a read-only mount never overwrites in place"
        );
    }
    squeezefs::fuse_client::set_inplace_overwrite(false);
}

/// The reclaim queue issues `BLKDISCARD` / `PUNCH_HOLE` against shared
/// hardware. A reader's reclaimer could discard a range the writer just
/// wrote (§6.3). Nothing is ever queued, and the queue is halted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_latch_refuses_reclaim_enqueue() {
    let q = ReclaimQueue::from_env();
    let ba = Arc::new(BlockAllocator::new("ro_reclaim_test").await.unwrap());
    let b = NamedTempFile::new().unwrap();
    b.as_file().set_len(8 * 1024 * 1024).unwrap();
    let inflight = ba.inflight_register(0);

    let _latch = RoLatch::arm();
    q.enqueue(ReclaimEntry {
        allocator: ba,
        inflight,
        device_path: b.path().to_string_lossy().to_string(),
        offset: 0,
        size: 4096,
    })
    .await;
    assert_eq!(
        q.queued_len(),
        0,
        "a read-only mount queues no device reclaims"
    );
}

/// `recover_active_blocks_v3`'s free-completing arm: the walk declares
/// every gap below the cursor FREE. On a reader — whose tree view is a
/// snapshot — that is a fabricated free list over a live writer's blocks.
/// It refuses instead of walking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_latch_refuses_the_recovery_walk() {
    let vol = fresh_volume().await;
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");
    let (router, _b) = data_router("ro_recover_test").await;

    let _latch = RoLatch::arm();
    let err = router
        .backend_router
        .default_allocator
        .recover_active_blocks_v3(reader.as_ref(), &router.backend_router)
        .await
        .expect_err("the recovery walk must refuse on a read-only mount")
        .to_string();
    assert!(err.contains("read-only"), "loud and named: {err}");
    assert_eq!(
        router.backend_router.default_allocator.free_blocks_count(),
        0,
        "no free list was fabricated"
    );
}

// ===========================================================================
// Item 1 — the operator surface (`-o ro` / `--read-only`)
// ===========================================================================

/// One resolution point for both spellings, and one refusal for the
/// contradiction (the `-o interception` + explicit-writeback precedent).
#[test]
fn read_only_option_resolution_and_conflict_refusal() {
    assert!(!read_only_from_options(None, false).unwrap());
    assert!(
        read_only_from_options(None, true).unwrap(),
        "--read-only alone"
    );
    assert!(read_only_from_options(Some("ro"), false).unwrap(), "-o ro");
    assert!(
        read_only_from_options(Some("allow_other,ro,nosuid"), false).unwrap(),
        "-o ro inside a list"
    );
    assert!(
        !read_only_from_options(Some("allow_other,rootmode=40000"), false).unwrap(),
        "a key merely CONTAINING `ro` is not `-o ro`"
    );
    assert!(
        !read_only_from_options(Some("rw"), false).unwrap(),
        "-o rw is the default posture"
    );
    let err = read_only_from_options(Some("rw"), true)
        .expect_err("--read-only with an explicit -o rw is a contradiction");
    assert!(err.contains("read-only") && err.contains("rw"), "{err}");
    let err =
        read_only_from_options(Some("ro,rw"), false).expect_err("-o ro,rw is a contradiction");
    assert!(err.contains("read-only") && err.contains("rw"), "{err}");
}

/// `ro` is applied through `MountOptions::read_only` (MS_RDONLY + the
/// `ro` fusermount option), so it must not ALSO be smuggled into the
/// custom kernel option string — the daemon-level keys are stripped there.
#[test]
fn ro_is_daemon_level_in_the_kernel_option_filter() {
    let kernel =
        squeezefs::fuse_client::filter_kernel_mount_options("ro,allow_other,max_read=4096");
    assert!(
        !kernel.split(',').any(|o| o.trim() == "ro"),
        "`ro` must not ride the custom kernel option string: {kernel}"
    );
    assert!(kernel.contains("allow_other") && kernel.contains("max_read=4096"));
}

// ===========================================================================
// Item 4 — TTL alignment to the reader's staleness bound
// ===========================================================================

/// The reader's coherence horizon is its **staleness bound** (the poll
/// interval plus the writer's ≤ 1 s checkpoint ceiling), so every kernel TTL
/// derives from that — no hardcoded 1 s, no hardcoded 300 s. A cache may not
/// hold an entry longer than the interval over which freshness can be
/// proven.
#[test]
fn read_only_kernel_ttls_derive_from_the_staleness_bound() {
    let bound = Duration::from_millis(2500);
    let t = KernelCacheTtls::read_only_defaults(bound);
    assert_eq!(t.attr, bound);
    assert_eq!(t.entry, bound);
    assert_eq!(t.dir_entry, bound);
    assert_eq!(t.negative, bound);

    // The write-mount default is untouched (readers must not retune
    // writers).
    let rw = KernelCacheTtls::default();
    assert_eq!(rw.attr, Duration::from_secs(1));
}

/// The wiring tie (drift-is-red): the TTL derivation source and the poll
/// cadence both come from the LANDED machinery, and the bound is exactly
/// the interval plus the writer's checkpoint ceiling. If the metadata
/// plane's cadence law changes, this test fails instead of the reader
/// silently advertising a window it no longer honours.
#[test]
fn the_reader_horizon_matches_the_landed_revalidation_machinery() {
    use squeezefs::meta_backend::kv::revalidate::RevalidationPoller;

    let poller = RevalidationPoller::derived();
    assert_eq!(
        squeezefs::ro_coherence::reader_revalidate_interval(),
        poller.interval(),
        "the reader's cadence IS the landed derived cadence"
    );
    assert_eq!(
        squeezefs::ro_coherence::reader_staleness_bound(),
        poller.staleness_bound(),
        "the advertised bound IS the machinery's bound"
    );
    assert_eq!(
        squeezefs::ro_coherence::reader_staleness_bound(),
        poller.interval() + Duration::from_secs(1),
        "bound = poll interval + the writer's <=1s checkpoint ceiling"
    );
    // And the TTLs a reader mounts with are that bound, not a constant.
    assert_eq!(
        KernelCacheTtls::read_only_defaults(squeezefs::ro_coherence::reader_staleness_bound()).attr,
        poller.staleness_bound()
    );
}

/// Precedence is unchanged (the env-knob law): the bound-derived value is a
/// DEFAULT, and an explicit `-o` value still wins verbatim — which is
/// exactly why the docs must say that setting one LENGTHENS the staleness
/// window by the amount it exceeds the bound.
#[test]
fn explicit_ttl_options_still_win_over_the_derived_reader_default() {
    let bound = Duration::from_millis(2000);
    let t = KernelCacheTtls::read_only_defaults(bound)
        .with_mount_options("attr_timeout=5,negative_timeout=0");
    assert_eq!(t.attr, Duration::from_secs(5), "explicit wins verbatim");
    assert!(t.attr > bound, "and it lengthens the window past the bound");
    assert_eq!(t.negative, Duration::ZERO);
    assert_eq!(
        t.entry, bound,
        "unspecified classes keep the derived reader default"
    );
}

/// `dir_entry_cache_v3`'s 300 s TTL is cut to the staleness bound for
/// readers (§6.8 item 4) and left alone for writers — and the reader's cut
/// can never EXCEED the shipped 300 s (a huge `--meta-flush-interval`
/// lengthens the bound and must not lengthen a cache past what a write
/// mount ships with).
#[test]
fn reader_daemon_cache_ttl_is_the_bound_capped_by_the_shipped_horizon() {
    assert_eq!(
        reader_daemon_cache_ttl(Duration::from_millis(2000)),
        Duration::from_millis(2000)
    );
    assert_eq!(
        reader_daemon_cache_ttl(Duration::from_secs(86_400)),
        Duration::from_secs(300),
        "never longer than the shipped dentry-cache horizon"
    );
}

// ===========================================================================
// Item 5 — purge on revalidation
// ===========================================================================

/// The invalidation primitive already exists and is complete
/// (`purge_block_key`, five stores, grep-guarded). S5 wires the TRIGGER:
/// a revalidation epoch that observed advanced roots purges the reader's
/// block-key stores through that one call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revalidation_epoch_purges_the_block_key_stores() {
    let (router, _b) = data_router("ro_purge_test").await;
    let cache = &router.cache;

    let key = "ro_purge_test://4194304";
    cache
        .read_lru
        .put(key, bytes::Bytes::from_static(&[7u8; 4096]));
    cache
        .hot_block
        .put(key, bytes::Bytes::from_static(&[7u8; 4096]));
    assert!(
        cache.read_lru.get(key).is_some(),
        "control: the key is warm"
    );

    let purged = squeezefs::ro_coherence::purge_reader_block_keys(cache);
    assert!(
        purged >= 1,
        "the revalidation purge must report the keys it dropped"
    );
    assert!(
        cache.read_lru.get(key).is_none(),
        "a revalidation epoch must drop the reader's cached block bytes \
         (they may name an offset the writer has since reallocated)"
    );
    assert!(
        cache.hot_block.get(key).is_none(),
        "the R4 hot tier is one of the five stores the purge covers"
    );
}

/// **The item-2 wiring, end to end.** An ARMED reader observes the writer's
/// checkpoint, adopts the new roots, drops the nodes the new roots do not
/// cover, and — through the installed sink — fires the R-6 purge over its
/// block-key census, all in one poll.
///
/// This is the test whose intent used to be "the poll reports a boolean the
/// item-2 arm will consume": the arm has landed, so the same intent is now
/// stated against the real epoch step rather than against a placeholder's
/// input. `poll_at` is driven with an explicit `Instant` — the landed
/// poller's own test seam — so the cadence never becomes a sleep here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_armed_reader_observes_the_writer_advancing_and_purges_its_tiers() {
    use squeezefs::meta_backend::kv::revalidate::RevalidationPoller;

    let vol = fresh_volume().await;
    // Settle a checkpoint so the reader arms at a known ledger record.
    let writer = KvMetaBackend::open(vol.path()).await.expect("write mount");
    writer.checkpoint_now().await.expect("initial checkpoint");

    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");
    let (router, _b) = data_router("ro_wiring_test").await;

    // A warm block key: the reader's data-plane state that an epoch step
    // must invalidate (§6.3's binding hazard — the offset may have been
    // freed and reallocated by the writer since we cached it).
    let key = "ro_wiring_test://4194304";
    router
        .cache
        .read_lru
        .put(key, bytes::Bytes::from_static(&[7u8; 4096]));

    // The item-2 DECLARATION + the item-5 sink, exactly as the mount does.
    assert_eq!(
        squeezefs::ro_coherence::arm_reader_coherence(std::slice::from_ref(&reader), &router),
        1,
        "the volume must arm as a coherent reader"
    );
    assert_eq!(
        reader.reader_epoch(),
        reader.mounted_ledger().seq,
        "arming adopts the record the mount opened — nothing is dropped by it"
    );

    // A poll with nothing committed is INERT: no epoch step, no drop, no
    // purge (an idle writer costs a reader nothing).
    let poller = RevalidationPoller::new(1);
    let inert = poller
        .poll_at(&reader, std::time::Instant::now())
        .await
        .expect("the poll must succeed")
        .expect("the poll was due");
    assert!(!inert.advanced, "nothing was committed");
    assert_eq!(inert.keys_purged, 0, "an inert poll purges nothing");
    assert!(
        router.cache.read_lru.get(key).is_some(),
        "and it leaves the reader's warm tiers alone"
    );

    // The writer commits and checkpoints: the roots move.
    Metadata::create(writer.as_ref(), 1, "new-file", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    writer.checkpoint_now().await.expect("checkpoint");

    let advanced = poller
        .poll_at(&reader, std::time::Instant::now())
        .await
        .expect("the poll must succeed")
        .expect("the poll was due");
    assert!(
        advanced.advanced,
        "the reader must observe the writer's new checkpoint (epoch {} → {})",
        advanced.from_epoch, advanced.epoch
    );
    assert!(advanced.epoch > inert.epoch, "epochs only advance");
    assert_eq!(
        reader.reader_epoch(),
        advanced.epoch,
        "the live epoch IS the adopted record"
    );
    // Item 5: the sink fired, through the ONE unified purge.
    assert!(
        advanced.keys_purged >= 1,
        "the epoch step must fire the R-6 purge — `keys_purged` at 0 while epochs \
         advance is a coherence promise that is silently not kept"
    );
    assert!(
        router.cache.read_lru.get(key).is_none(),
        "the reader's cached block bytes must be gone after the epoch step"
    );

    writer.shutdown().await.unwrap();
}

/// Arming is a once-per-mount DECLARATION: a second attempt on the same
/// volume is refused (the landed contract) and the mount path reports it as
/// "not armed" rather than double-installing a sink. The refusal is loud but
/// never fatal — that volume keeps serving its current epoch, which is stale
/// but never wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arming_a_reader_twice_is_refused_not_double_installed() {
    let vol = fresh_volume().await;
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");
    let (router, _b) = data_router("ro_rearm_test").await;
    let vols = std::slice::from_ref(&reader);

    assert_eq!(
        squeezefs::ro_coherence::arm_reader_coherence(vols, &router),
        1,
        "first arm declares the reader"
    );
    assert_eq!(
        squeezefs::ro_coherence::arm_reader_coherence(vols, &router),
        0,
        "a second arm is refused — one declaration per mount"
    );
    // And the volume is still a working reader on its first declaration.
    assert_eq!(reader.reader_epoch(), reader.mounted_ledger().seq);
}

/// The mount's sink is the CENSUS pass, not the registered-suspect drain —
/// deliberately, because no registration site exists yet, and a sink that
/// purges only registrations would leave `meta_kv_revalidate_keys_purged`
/// at 0 while epochs advanced (a silently broken promise). Pinned directly
/// on the sink so the choice cannot be reverted by accident.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mount_sink_purges_the_census_without_any_registration() {
    use squeezefs::meta_backend::kv::node_cache::EpochPurgeSink;

    let (router, _b) = data_router("ro_sink_test").await;
    for i in 0..3u64 {
        router.cache.read_lru.put(
            &format!("ro_sink_test://{}", i * 4_194_304),
            bytes::Bytes::from_static(&[1u8; 4096]),
        );
    }
    let sink = squeezefs::ro_coherence::ReaderEpochPurge::new(router.clone());
    // No `note_suspect`-style registration happened, and it still purges.
    assert_eq!(
        sink.on_epoch_advance(7, 8),
        3,
        "the census sink purges what the reader actually cached"
    );
    assert!(router.cache.read_lru.get("ro_sink_test://0").is_none());
}

/// The revalidation task must EXIT at dismount (no leaked tasks — and the
/// stop authority is the `dismount_once` FLAG, not the notify: a
/// `notify_waiters()` that fires between two of the loop's registrations
/// is lost, so using the notify as the authority would strand the task for
/// the process's life).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_revalidation_task_exits_at_dismount() {
    let vol = fresh_volume().await;
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let wake = Arc::new(squeezefs_ipc::sqz_notify::Notify::new());
    let handle = squeezefs::ro_coherence::spawn_reader_revalidation(
        vec![reader],
        stop.clone(),
        wake.clone(),
    );

    // Dismount: latch first, then wake — the same order the mount path
    // uses (`dismount_once.swap(true)` precedes `notify_waiters`).
    stop.store(true, std::sync::atomic::Ordering::Release);
    wake.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the revalidation task must exit at dismount, not leak")
        .expect("and it must not panic");
}

/// **Item 6 at the FUSE door.** The kernel refuses mutations on an
/// `MS_RDONLY` mount, but an interception client's ring writes never
/// traverse the VFS — so the daemon-side handler gate is what actually
/// holds, and it must answer `EROFS` (what applications and every POSIX
/// suite expect from a read-only filesystem).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fuse_mutating_handlers_answer_erofs_on_a_reader() {
    use fuse3::raw::prelude::Filesystem;

    let vol = fresh_volume().await;
    let (router, _b) = data_router("ro_erofs_test").await;
    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    let mut fs = squeezefs::fuse_client::SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    let routed = {
        let be = KvMetaBackend::open(vol.path()).await.unwrap();
        Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = fuse3::raw::Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };

    // Control: writable.
    let created = fs
        .create(
            req,
            1,
            std::ffi::OsStr::new("writable"),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .expect("control: a write mount creates");

    let _latch = RoLatch::arm();
    let erofs = fuse3::Errno::from(libc::EROFS);
    assert_eq!(
        fs.create(
            req,
            1,
            std::ffi::OsStr::new("nope"),
            libc::S_IFREG | 0o644,
            0
        )
        .await
        .err(),
        Some(erofs),
        "create must answer EROFS"
    );
    assert_eq!(
        fs.mkdir(req, 1, std::ffi::OsStr::new("d"), 0o755, 0)
            .await
            .err(),
        Some(erofs),
        "mkdir must answer EROFS"
    );
    assert_eq!(
        fs.unlink(req, 1, std::ffi::OsStr::new("writable"))
            .await
            .err(),
        Some(erofs),
        "unlink must answer EROFS"
    );
    assert_eq!(
        fs.write(
            req,
            created.attr.ino,
            0,
            0,
            bytes::Bytes::from_static(b"x"),
            0,
            0
        )
        .await
        .err(),
        Some(erofs),
        "write must answer EROFS — the gate an interception ring write hits"
    );
    assert_eq!(
        fs.setxattr(
            req,
            created.attr.ino,
            std::ffi::OsStr::new("user.x"),
            b"1",
            0,
            0
        )
        .await
        .err(),
        Some(erofs),
        "setxattr must answer EROFS"
    );
    assert_eq!(
        fs.open(req, created.attr.ino, libc::O_WRONLY as u32, 0)
            .await
            .err(),
        Some(erofs),
        "write INTENT must be refused at open, where POSIX programs check"
    );
    // Reads keep working — that is the entire point of a reader.
    fs.open(req, created.attr.ino, libc::O_RDONLY as u32, 0)
        .await
        .expect("O_RDONLY still opens on a reader");
    fs.lookup(req, 1, std::ffi::OsStr::new("writable"))
        .await
        .expect("lookup still serves on a reader");
}

/// The latch is a single relaxed load and defaults OFF — a write mount
/// must never observe it armed (the reader feature taxes no writer).
#[test]
fn the_read_only_latch_defaults_off() {
    assert!(
        !read_only_mount(),
        "the RO latch must default off — a write mount pays one \
         never-taken branch, nothing more"
    );
}

// ===========================================================================
// The rung-6 fleet findings (2026-08-15) — the repro-port mandate's cargo
// pins. Found live by tests/mw_fleet.sh + tests/run_mw_matrix.sh smoke.
// ===========================================================================

/// **Rung-6 finding #1 — the reader dirty-tail pin.** A reader that
/// bootstraps while the writer's journal tail is non-empty replays that
/// tail as DIRTY RAM records (`apply_replayed` rides the commit path's
/// `apply_locked`, which lowers `dirty_floor`). On a WRITER that is
/// correct — its checkpoint task flushes them — but a reader can never
/// checkpoint: the records are the writer's to persist. Un-absolved, the
/// S5 drop pass refuses those nodes FOREVER (`meta_kv_revalidate_dirty_skips`
/// — a must-stay-0 tripwire — climbs on every epoch) and the reader's view
/// of them freezes at mount-time state: an UNBOUNDED violation of the
/// published staleness bound.
///
/// The contract pinned here: the reader DECLARATION
/// (`arm_reader_revalidation`) absolves the bootstrap replay's dirty
/// residue, so the drop pass treats those nodes like any clean node —
/// `dirty_skips` stays 0, and both the replayed-tail record and post-mount
/// writes become visible after one epoch step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_bootstrap_into_a_dirty_journal_tail_never_pins_nodes() {
    use squeezefs::meta_backend::kv::revalidate::{revalidation_stats, RevalidationPoller};

    let vol = fresh_volume().await;
    let writer = KvMetaBackend::open(vol.path()).await.expect("write mount");
    Metadata::create(writer.as_ref(), 1, "pre-tail", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("pre-tail create");
    writer.checkpoint_now().await.expect("settle checkpoint");

    // A committed-but-uncheckpointed transaction: the journal tail the
    // reader bootstraps into. The writer's ≤1 s cadence can race this
    // fixture on a loaded box, so the dirty-bootstrap CONTROL below
    // retries with a fresh tail rather than passing vacuously.
    let mut reader = None;
    for attempt in 0..3 {
        Metadata::create(
            writer.as_ref(),
            1,
            &format!("in-tail-{attempt}"),
            libc::S_IFREG | 0o644,
            0,
            0,
        )
        .await
        .expect("in-tail create");
        let r = KvMetaBackend::open_read_only(vol.path())
            .await
            .expect("read-only mount");
        if r.node_cache_gauge().1 > 0 {
            reader = Some((r, attempt));
            break;
        }
        // The writer checkpointed under us — the tail was empty. Re-roll.
    }
    let (reader, attempt) = reader.expect(
        "control: the bootstrap replay never observed a dirty journal tail \
         across 3 attempts — the fixture lost every race to the writer's \
         checkpoint cadence (re-run; this is the fixture, not the product)",
    );

    let before = revalidation_stats();
    reader
        .arm_reader_revalidation(None)
        .expect("the reader declaration");
    assert_eq!(
        reader.node_cache_gauge().1,
        0,
        "arming reader revalidation must ABSOLVE the bootstrap replay's \
         dirty residue: a reader has no checkpoint task, so a kept dirty \
         floor can never be discharged and pins the node forever"
    );

    // The writer moves on: a post-mount create, then a checkpoint the
    // reader will observe as an epoch step.
    Metadata::create(
        writer.as_ref(),
        1,
        "post-mount",
        libc::S_IFREG | 0o644,
        0,
        0,
    )
    .await
    .expect("post-mount create");
    writer.checkpoint_now().await.expect("checkpoint");

    let poller = RevalidationPoller::new(1);
    let out = poller
        .poll_at(&reader, std::time::Instant::now())
        .await
        .expect("the poll must succeed")
        .expect("the poll was due");
    assert!(out.advanced, "the reader must observe the new checkpoint");
    assert_eq!(
        out.skipped_dirty, 0,
        "the drop pass must not meet reader-replayed dirty nodes \
         (the rung-6 pinned-node shape)"
    );
    let after = revalidation_stats();
    assert_eq!(
        after.dirty_skips - before.dirty_skips,
        0,
        "meta_kv_revalidate_dirty_skips is a must-stay-0 tripwire on EVERY \
         posture — it may only ever mean 'revalidation armed on a mount \
         that writes', never 'a reader bootstrapped into a journal tail'"
    );

    // Convergence, both faces: the replayed-tail record is still resolvable
    // (the checkpoint the epoch adopted covers the very window the replay
    // read) and the post-mount write became visible.
    Metadata::lookup(reader.as_ref(), 1, &format!("in-tail-{attempt}"))
        .await
        .expect("the replayed-tail record must survive the epoch step");
    Metadata::lookup(reader.as_ref(), 1, "post-mount")
        .await
        .expect("a post-mount write must become visible within one epoch step");

    writer.shutdown().await.expect("writer shutdown");
}

/// **Rung-6 finding #2 — multi-meta-volume live-reader non-convergence**
/// (the field-relevant one: production sets run 4 meta volumes). On a
/// 2-volume set a LIVE reader never observed post-mount content: readdir
/// listed the new dentry while lookup/getattr of the child ENOENT'd
/// forever (`d?????????` in ls -la), with revalidation epochs advancing
/// and `dirty_skips == 0`, while a FRESH reader resolved the same records
/// instantly.
///
/// Root cause: `RevalidationPoller`'s cadence mark (`last`) is per-POLLER
/// state, and the S5 task drove one shared poller through a per-volume
/// `poll_at` loop at one pass instant — volume 0's `mark` made `due_at`
/// false for every sibling on the same pass, every pass, forever. Only
/// meta volume 0 ever revalidated: the parent's dentry tree (volume 0)
/// advanced while the child's inode record (volume 1) stayed frozen at
/// the reader's mount-time snapshot — exactly the readdir-sees/
/// lookup-misses split observed live.
///
/// The contract pinned here: ONE pass over the set makes ONE cadence
/// decision and polls EVERY volume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_reader_set_revalidates_every_meta_volume() {
    use squeezefs::meta_backend::kv::builder::format_v3_stamped;
    use squeezefs::meta_backend::kv::revalidate::RevalidationPoller;
    use squeezefs::meta_backend::{
        open_routed_meta_set, open_routed_meta_set_read_only, plan_meta_slot_set_with_width,
    };

    const SET_VOL_LEN: u64 = 128 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let metas: Vec<std::path::PathBuf> = (0..2)
        .map(|i| {
            let p = dir.path().join(format!("meta{i}"));
            std::fs::File::create(&p)
                .unwrap()
                .set_len(SET_VOL_LEN)
                .unwrap();
            p
        })
        .collect();
    let plan = plan_meta_slot_set_with_width(metas.len(), 8).expect("slot plan");
    let set_opts = squeezefs::meta_backend::kv::builder::FormatV3Options {
        node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    };
    for (i, m) in metas.iter().enumerate() {
        format_v3_stamped(m, SET_VOL_LEN, &set_opts, plan.stamps[i].clone())
            .await
            .expect("format stamped set member");
    }
    let paths: Vec<String> = metas.iter().map(|m| m.display().to_string()).collect();

    let writer = open_routed_meta_set(&paths).await.expect("writer set");
    // Settle every volume so the reader bootstraps an empty journal tail —
    // this pin must stay independent of the dirty-tail finding above.
    for vol in &writer.volumes {
        vol.checkpoint_now().await.expect("settle checkpoint");
    }

    // The LIVE reader mounts BEFORE the content exists (the field shape).
    let reader = open_routed_meta_set_read_only(&paths)
        .await
        .expect("reader set");
    for vol in &reader.volumes {
        vol.arm_reader_revalidation(None).expect("arm volume");
    }
    let armed_epochs: Vec<u64> = reader.volumes.iter().map(|v| v.reader_epoch()).collect();

    // Post-mount content on EVERY volume (the mint rotor round-robins the
    // healthy set, so a handful of creates covers both), then checkpoint
    // the whole set.
    let mut name_on_vol: Vec<Option<(String, u64)>> = vec![None; writer.volumes.len()];
    for i in 0..32 {
        let name = format!("f{i}");
        let ino = writer
            .create(1, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        let (v_idx, _) = writer.route_ino(ino);
        if name_on_vol[v_idx].is_none() {
            name_on_vol[v_idx] = Some((name, ino));
        }
        if name_on_vol.iter().all(|x| x.is_some()) {
            break;
        }
    }
    let names: Vec<(String, u64)> = name_on_vol
        .into_iter()
        .map(|x| x.expect("the mint rotor must reach every volume of the set"))
        .collect();
    for vol in &writer.volumes {
        vol.checkpoint_now().await.expect("checkpoint");
    }

    // ONE revalidation pass over the SET, exactly as the mount task drives
    // it: one shared poller, one pass instant.
    let poller = RevalidationPoller::new(1);
    let outcomes = poller
        .poll_set_at(&reader.volumes, std::time::Instant::now())
        .await;
    assert_eq!(
        outcomes.len(),
        reader.volumes.len(),
        "one pass must POLL every volume of the set — a pass whose cadence \
         mark suppresses the siblings leaves their inode planes frozen \
         FOREVER (rung-6 finding #2)"
    );
    for (idx, res) in &outcomes {
        let out = res.as_ref().expect("per-volume poll must succeed");
        assert!(
            out.advanced,
            "volume {idx} must adopt the writer's new checkpoint in this pass"
        );
    }
    for (i, v) in reader.volumes.iter().enumerate() {
        assert!(
            v.reader_epoch() > armed_epochs[i],
            "volume {i} never advanced past its arm-time epoch — the \
             readdir-sees/lookup-misses split (only volume 0 revalidating)"
        );
    }

    // The observed field faces, both healed within ONE pass: the dentry
    // resolves AND the child inode record resolves, on every volume.
    for (name, ino) in &names {
        let hit = reader
            .lookup(1, name)
            .await
            .unwrap_or_else(|e| panic!("lookup of {name} must resolve on the live reader: {e}"));
        assert_eq!(hit.ino, *ino, "the dentry names the created ino");
        reader.getattr(*ino).await.unwrap_or_else(|e| {
            panic!(
                "getattr of ino {ino:#x} ({name}) must resolve — ENOENT here \
                 is the live field shape (d????????? in ls -la): {e}"
            )
        });
    }

    for vol in &writer.volumes {
        vol.shutdown().await.expect("volume shutdown");
    }
}

// ===========================================================================
// PR 5 — the S5 reader RE-SCOPED to tokens (design-symmetric-metadata
// §5.7.2, R-SYM-4 / KD-SYM-19): under `SQUEEZEFS_SYMMETRIC_META=1` a
// `-o ro` mount is a member-reader + TOKEN CLIENT — user-visible metadata
// is exact at the next resolve, so its bound reads 0 and every TTL derives
// from that; every contract above is the `=0` posture, kept verbatim.
// ===========================================================================

/// The knobs are process-global; the token-posture contracts serialize on
/// this (the `RoLatch` convention — one mount per daemon process).
static TOKEN_POSTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct SymmetricKnob;

impl SymmetricKnob {
    fn on() -> Self {
        std::env::set_var(
            squeezefs::meta_backend::kv::slot_lease::SYMMETRIC_META_ENV,
            "1",
        );
        Self
    }
}

impl Drop for SymmetricKnob {
    fn drop(&mut self) {
        std::env::remove_var(squeezefs::meta_backend::kv::slot_lease::SYMMETRIC_META_ENV);
    }
}

async fn stamped_volume() -> NamedTempFile {
    use squeezefs::meta_backend::kv::builder::format_v3_stamped;
    let f = NamedTempFile::new().unwrap();
    f.as_file().set_len(VOL_LEN).unwrap();
    let plan = squeezefs::meta_backend::plan_meta_slot_set(1).expect("derived plan");
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let r = format_v3_stamped(f.path(), VOL_LEN, &opts(), plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format the stamped volume");
    f
}

/// Item 4 re-scoped: a TOKEN reader's user-visible metadata bound is
/// **0** — a cache may hold an entry exactly as long as freshness is
/// proven, and under a token that is "until the recall", which no TTL can
/// express — so every kernel TTL class and the daemon dentry/attr horizon
/// derive to 0 and every resolve reaches the token cache. The S5 poller's
/// own bound is untouched (it keeps its control-plane meaning), and with
/// the knob off the reader's TTLs are the S5 derivation verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_readers_metadata_bound_is_zero_and_its_ttls_derive_from_it() {
    let _serial = TOKEN_POSTURE.lock().await;
    let _latch = RoLatch::arm();
    assert!(
        !squeezefs::ro_coherence::token_reader_requested(),
        "a plain -o ro mount is the S5 poller (the knob is off)"
    );
    assert_eq!(
        squeezefs::ro_coherence::metadata_staleness_bound(),
        squeezefs::ro_coherence::reader_staleness_bound(),
        "knob off: the S5 bound verbatim"
    );
    let _knob = SymmetricKnob::on();
    assert!(squeezefs::ro_coherence::token_reader_requested());
    assert_eq!(
        squeezefs::ro_coherence::metadata_staleness_bound(),
        Duration::ZERO,
        "under tokens the user-visible metadata bound is exact — 0"
    );
    assert_ne!(
        squeezefs::ro_coherence::reader_staleness_bound(),
        Duration::ZERO,
        "the S5 poll keeps its own bound as the control-plane cadence"
    );
    let t =
        KernelCacheTtls::read_only_defaults(squeezefs::ro_coherence::metadata_staleness_bound());
    assert_eq!(t.attr, Duration::ZERO);
    assert_eq!(t.entry, Duration::ZERO);
    assert_eq!(t.dir_entry, Duration::ZERO);
    assert_eq!(t.negative, Duration::ZERO);
    assert_eq!(
        reader_daemon_cache_ttl(squeezefs::ro_coherence::metadata_staleness_bound()),
        Duration::ZERO,
        "the daemon dentry/attr caches hold nothing across a recall"
    );
    // The stats face agrees with the mount's request before any plane is
    // armed (the two are one law, not two).
    assert_eq!(squeezefs::ro_coherence::metadata_staleness_bound_ms(&[]), 0);
}

/// The mount-path arm under `SQUEEZEFS_SYMMETRIC_META=0` is the S5 reader
/// EXACTLY: nothing armed, no token plane on any volume, `Ok(0)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mount_path_token_arm_is_the_s5_reader_verbatim_when_the_knob_is_off() {
    let _serial = TOKEN_POSTURE.lock().await;
    let _latch = RoLatch::arm();
    let vol = stamped_volume().await;
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");
    let (router, _b) = data_router("ro_token_off").await;
    let armed = squeezefs::ro_coherence::arm_token_readers(std::slice::from_ref(&reader), &router)
        .await
        .expect("the knob off arms nothing and refuses nothing");
    assert_eq!(armed, 0);
    assert!(reader.token_reader().is_none());
    assert_eq!(
        squeezefs::ro_coherence::metadata_staleness_bound_ms(std::slice::from_ref(&reader)),
        squeezefs::ro_coherence::reader_staleness_bound().as_millis() as u64
    );
}

/// `=1` on a bit-17-ABSENT volume refuses the READER's open too, naming
/// the conversion verb — the writer's law (PR 4) has one reader face:
/// there is no holder to grant a token on a flat volume, and a reader
/// that silently fell back to the poll would be the second method
/// R-SYM-4 forbids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mount_path_token_arm_refuses_loud_on_a_flat_volume() {
    let _serial = TOKEN_POSTURE.lock().await;
    let _latch = RoLatch::arm();
    let vol = fresh_volume().await;
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");
    let (router, _b) = data_router("ro_token_flat").await;
    let _knob = SymmetricKnob::on();
    let err = squeezefs::ro_coherence::arm_token_readers(std::slice::from_ref(&reader), &router)
        .await
        .expect_err("a flat volume cannot serve tokens");
    assert!(
        err.contains("enable-symmetric"),
        "the refusal names the remedy: {err}"
    );
    assert!(reader.token_reader().is_none(), "nothing armed on refusal");
}

/// A token client IS a member-reader: the holder judges an unacked
/// recall by the reader's membership lease, so a reader with no lease
/// would be treated as dead at every recall while serving from its cache
/// — the posture refuses loud instead, naming the plane the writer must
/// arm. A reader with a lease and no declared holder endpoint refuses
/// naming the endpoint knob.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mount_path_token_arm_refuses_loud_without_a_membership_lease() {
    let _serial = TOKEN_POSTURE.lock().await;
    let _latch = RoLatch::arm();
    let vol = stamped_volume().await;
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");
    let (router, _b) = data_router("ro_token_lease").await;
    let _knob = SymmetricKnob::on();
    let err = squeezefs::ro_coherence::arm_token_readers(std::slice::from_ref(&reader), &router)
        .await
        .expect_err("no membership lease ⇒ no token client");
    assert!(
        err.contains("SQUEEZEFS_MEMBERSHIP_BIND"),
        "the refusal names the plane the writer arms: {err}"
    );
    assert!(reader.token_reader().is_none(), "nothing armed on refusal");
}

// ===========================================================================
// PR 5 — the predicted-slot-first ledger read (design-symmetric-metadata
// §5.7 title item; the control-plane poll's economy)
// ===========================================================================

/// The poll reads the 4 KiB slot the writer's NEXT record must land in
/// (`(mounted seq + 1) % 32` — the round-robin law) before anything else:
/// an idle poll is ONE 4 KiB read that finds an older record and stops; a
/// poll after k checkpoints reads k + 1 slots (each successor, then the
/// slot that holds an older record); the 128 KiB whole-ledger read
/// survives as the FALLBACK for a torn predicted slot (the writer mid-write
/// — the read finds the newer record in any slot) and is counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_poll_reads_the_predicted_slot_first_and_falls_back_on_a_torn_one() {
    use squeezefs::meta_backend::kv::checkpoint::{ROOT_LEDGER_SLOTS, ROOT_LEDGER_SLOT_LEN};
    use squeezefs::meta_backend::kv::revalidate::revalidation_stats;

    let vol = fresh_volume().await;
    let writer = KvMetaBackend::open(vol.path()).await.expect("writer");
    Metadata::create(writer.as_ref(), 1, "a", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    writer.checkpoint_now().await.expect("checkpoint");
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("reader");
    reader.arm_reader_revalidation(None).expect("arm");
    let slot_len = ROOT_LEDGER_SLOT_LEN;

    // An idle poll: exactly one predicted slot.
    let s0 = revalidation_stats();
    let out = reader.revalidate_reader().await.expect("poll");
    assert!(!out.advanced, "nothing to adopt");
    let s1 = revalidation_stats();
    assert_eq!(
        s1.ledger_read_bytes - s0.ledger_read_bytes,
        slot_len,
        "an idle poll reads ONE 4 KiB slot"
    );
    assert_eq!(s1.ledger_full_reads, s0.ledger_full_reads, "no fallback");

    // Two writer checkpoints, one poll: the two successors + the stop slot.
    for name in ["b", "c"] {
        Metadata::create(writer.as_ref(), 1, name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        writer.checkpoint_now().await.expect("checkpoint");
    }
    let out = reader.revalidate_reader().await.expect("poll");
    assert!(out.advanced, "two checkpoints to adopt");
    let s2 = revalidation_stats();
    assert_eq!(
        s2.ledger_read_bytes - s1.ledger_read_bytes,
        3 * slot_len,
        "k new checkpoints cost k + 1 slot reads"
    );
    assert_eq!(s2.ledger_full_reads, s1.ledger_full_reads);
    assert!(
        Metadata::lookup(reader.as_ref(), 1, "c").await.is_ok(),
        "the adopted epoch serves the newest checkpoint"
    );

    // A torn predicted slot: the writer advances once more, then the
    // slot its record occupies is overwritten with garbage — the poll
    // falls back to the whole-ledger read, which finds the newest record
    // anyway (here the torn one IS the newest, so the reader stays put).
    Metadata::create(writer.as_ref(), 1, "d", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    writer.checkpoint_now().await.expect("checkpoint");
    let mounted_seq = reader.reader_epoch();
    let predicted = (mounted_seq + 1) % ROOT_LEDGER_SLOTS;
    let at = reader.superblock().root_ledger.start + predicted * slot_len;
    {
        use std::io::{Seek, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(vol.path())
            .unwrap();
        f.seek(std::io::SeekFrom::Start(at)).unwrap();
        f.write_all(&vec![0xA5u8; slot_len as usize]).unwrap();
        f.sync_all().unwrap();
    }
    let out = reader.revalidate_reader().await.expect("poll");
    assert!(!out.advanced, "the only newer record is the torn one");
    let s3 = revalidation_stats();
    assert_eq!(
        s3.ledger_full_reads - s2.ledger_full_reads,
        1,
        "a torn predicted slot falls back to the whole-ledger read"
    );
    assert_eq!(
        s3.ledger_read_bytes - s2.ledger_read_bytes,
        slot_len + ROOT_LEDGER_SLOTS * slot_len,
        "the predicted slot, then the 128 KiB fallback"
    );
    // The writer's next checkpoint lands in the following slot; the
    // predicted slot is still the torn one, so the fallback finds it —
    // and once adopted, the prediction is current again (one slot per
    // idle poll).
    Metadata::create(writer.as_ref(), 1, "e", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    writer.checkpoint_now().await.expect("checkpoint");
    let out = reader.revalidate_reader().await.expect("poll");
    assert!(out.advanced, "the record after the torn slot is adopted");
    assert!(Metadata::lookup(reader.as_ref(), 1, "e").await.is_ok());
    let s4 = revalidation_stats();
    assert_eq!(s4.ledger_full_reads - s3.ledger_full_reads, 1);
    let out = reader.revalidate_reader().await.expect("poll");
    assert!(!out.advanced);
    let s5 = revalidation_stats();
    assert_eq!(s5.ledger_read_bytes - s4.ledger_read_bytes, slot_len);
    assert_eq!(s5.ledger_full_reads, s4.ledger_full_reads);
    writer.shutdown().await.expect("shutdown");
}

/// The token ack law depends on no lever (review round 1, Issue 6): a
/// `-o ro` mount under the knob with `SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED=0`
/// or `SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP=0` refuses LOUD naming the
/// lever — under S5 those `0` postures were safe because the ring's timers
/// stood behind them; under tokens the recall is the qualification and
/// there is no timer. The levers govern the `=0` reader alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mount_path_token_arm_refuses_a_drain_lever_at_zero() {
    let _serial = TOKEN_POSTURE.lock().await;
    let _latch = RoLatch::arm();
    let vol = stamped_volume().await;
    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");
    let (router, _b) = data_router("ro_token_levers").await;
    let _knob = SymmetricKnob::on();
    squeezefs::ro_coherence::test_set_drain_observed(Some(false));
    let err = squeezefs::ro_coherence::arm_token_readers(std::slice::from_ref(&reader), &router)
        .await
        .expect_err("the observed drain is not optional under tokens");
    assert!(
        err.contains("SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED"),
        "names the lever: {err}"
    );
    squeezefs::ro_coherence::test_set_drain_observed(None);
    squeezefs::ro_coherence::test_set_drain_epoch_stamp(Some(false));
    let err = squeezefs::ro_coherence::arm_token_readers(std::slice::from_ref(&reader), &router)
        .await
        .expect_err("the layout-cache step is not optional under tokens");
    assert!(
        err.contains("SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP"),
        "names the lever: {err}"
    );
    squeezefs::ro_coherence::test_set_drain_epoch_stamp(None);
    assert!(reader.token_reader().is_none(), "nothing armed on refusal");
}

/// An explicit non-zero kernel TTL on a token reader is refused (review
/// round 1, Issue 13): every class derives to 0 under tokens, and an
/// explicit `-o attr_timeout=` / `SQUEEZEFS_FUSE_*_TTL_MS` would re-create
/// a bounded-staleness dcache — the second read method R-SYM-4 forbids,
/// by lever. Zero TTLs pass; with the knob off the check is inert.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_kernel_ttl_on_a_token_reader_is_refused_loud() {
    let _serial = TOKEN_POSTURE.lock().await;
    let _latch = RoLatch::arm();
    let zero = KernelCacheTtls::read_only_defaults(Duration::ZERO);
    let lengthened = zero.with_mount_options("attr_timeout=1");
    assert!(
        squeezefs::ro_coherence::refuse_explicit_ttls_under_tokens(&lengthened).is_ok(),
        "knob off: the S5 precedence stands"
    );
    let _knob = SymmetricKnob::on();
    assert!(squeezefs::ro_coherence::refuse_explicit_ttls_under_tokens(&zero).is_ok());
    let err = squeezefs::ro_coherence::refuse_explicit_ttls_under_tokens(&lengthened)
        .expect_err("a non-zero TTL under tokens is refused");
    assert!(err.contains("attr_timeout"), "names the class: {err}");
    assert!(err.contains("R-SYM-4"), "names the law: {err}");
    let negative = zero.with_mount_options("negative_timeout=0.5");
    let err = squeezefs::ro_coherence::refuse_explicit_ttls_under_tokens(&negative)
        .expect_err("every class");
    assert!(err.contains("negative_timeout"), "{err}");
}
