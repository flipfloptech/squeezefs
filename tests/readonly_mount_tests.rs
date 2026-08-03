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
    read_only_from_options, read_only_mount, reader_daemon_cache_ttl, reader_revalidate_interval,
    set_read_only_mount, KernelCacheTtls,
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
    (
        squeezefs::routing::DataRouter::new(dlm, cache, ba, dev),
        b,
    )
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

    let writer = KvMetaBackend::open(vol.path())
        .await
        .expect("a write mount must NOT be refused because a reader is attached (D0 orthogonality)");
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
        .err()
        .expect("a second WRITER must still be refused (D0)")
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
        .err()
        .expect("create must refuse on a read-only mount")
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
        .err()
        .expect("a fresh foreign claim still refuses a WRITE mount")
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
        .err()
        .expect("allocation must refuse on a read-only mount")
        .to_string();
    assert!(
        err.contains("read-only"),
        "the refusal must name the read-only mount: {err}"
    );
    assert!(ba.allocate_block_below(64).is_none(), "contiguity picks too");
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
        .err()
        .expect("the recovery walk must refuse on a read-only mount")
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
        .err()
        .expect("--read-only with an explicit -o rw is a contradiction");
    assert!(err.contains("read-only") && err.contains("rw"), "{err}");
    let err = read_only_from_options(Some("ro,rw"), false)
        .err()
        .expect("-o ro,rw is a contradiction");
    assert!(err.contains("read-only") && err.contains("rw"), "{err}");
}

/// `ro` is applied through `MountOptions::read_only` (MS_RDONLY + the
/// `ro` fusermount option), so it must not ALSO be smuggled into the
/// custom kernel option string — the daemon-level keys are stripped there.
#[test]
fn ro_is_daemon_level_in_the_kernel_option_filter() {
    let kernel = squeezefs::fuse_client::filter_kernel_mount_options("ro,allow_other,max_read=4096");
    assert!(
        !kernel.split(',').any(|o| o.trim() == "ro"),
        "`ro` must not ride the custom kernel option string: {kernel}"
    );
    assert!(kernel.contains("allow_other") && kernel.contains("max_read=4096"));
}

// ===========================================================================
// Item 4 — TTL alignment to the checkpoint cadence
// ===========================================================================

/// The reader's coherence horizon IS the checkpoint cadence, so every
/// kernel TTL derives from it — no hardcoded 1 s, no hardcoded 300 s.
#[test]
fn read_only_kernel_ttls_derive_from_the_checkpoint_cadence() {
    let cadence = Duration::from_millis(250);
    let t = KernelCacheTtls::read_only_defaults(cadence);
    assert_eq!(t.attr, cadence);
    assert_eq!(t.entry, cadence);
    assert_eq!(t.dir_entry, cadence);
    assert_eq!(t.negative, cadence);

    // The write-mount default is untouched (readers must not retune
    // writers).
    let rw = KernelCacheTtls::default();
    assert_eq!(rw.attr, Duration::from_secs(1));

    // Strict mode (cadence 0 — checkpoint per commit) means the kernel
    // may cache nothing: honest, not clamped to a constant.
    assert_eq!(
        KernelCacheTtls::read_only_defaults(Duration::ZERO).attr,
        Duration::ZERO
    );
}

/// Precedence is unchanged (the env-knob law): the cadence-derived value
/// is a DEFAULT, and an explicit `-o` value still wins verbatim.
#[test]
fn explicit_ttl_options_still_win_over_the_derived_reader_default() {
    let t = KernelCacheTtls::read_only_defaults(Duration::from_millis(50))
        .with_mount_options("attr_timeout=5,negative_timeout=0");
    assert_eq!(t.attr, Duration::from_secs(5), "explicit wins verbatim");
    assert_eq!(t.negative, Duration::ZERO);
    assert_eq!(
        t.entry,
        Duration::from_millis(50),
        "unspecified classes keep the derived reader default"
    );
}

/// `dir_entry_cache_v3`'s 300 s TTL is cut to the cadence for readers
/// (§6.8 item 4) and left alone for writers — and the reader's cut can
/// never EXCEED the shipped 300 s (a huge `--meta-flush-interval` must not
/// lengthen a cache).
#[test]
fn reader_daemon_cache_ttl_is_the_cadence_capped_by_the_shipped_horizon() {
    assert_eq!(
        reader_daemon_cache_ttl(Duration::from_millis(50)),
        Duration::from_millis(50)
    );
    assert_eq!(
        reader_daemon_cache_ttl(Duration::from_secs(86_400)),
        Duration::from_secs(300),
        "never longer than the shipped dentry-cache horizon"
    );
}

/// The revalidation cadence is the checkpoint cadence, floored by a
/// PHYSICAL minimum (one 4 KiB ledger read per volume per pass) so strict
/// mode cannot spin the poll.
#[test]
fn reader_revalidate_interval_derives_from_the_cadence_with_a_physical_floor() {
    assert_eq!(
        reader_revalidate_interval(Duration::from_millis(200)),
        Duration::from_millis(200)
    );
    let floored = reader_revalidate_interval(Duration::ZERO);
    assert!(
        floored >= Duration::from_millis(1) && floored <= Duration::from_millis(50),
        "strict mode floors to a physical minimum, got {floored:?}"
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
    assert!(cache.read_lru.get(key).is_some(), "control: the key is warm");

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

/// The reader's revalidation pass polls the A/B root ledger (one 4 KiB
/// read) and reports whether the roots ADVANCED past the snapshot this
/// mount is serving. That boolean is the trigger for items 4 and 5, and
/// the input the node-cache revalidation arm (item 2, `feat/mw-node-cache-
/// coherence`) consumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revalidation_pass_observes_the_writer_advancing_the_roots() {
    let vol = fresh_volume().await;
    // Settle a checkpoint so the reader mounts on a known ledger seq.
    let writer = KvMetaBackend::open(vol.path()).await.expect("write mount");
    writer.checkpoint_now().await.expect("initial checkpoint");

    let reader = KvMetaBackend::open_read_only(vol.path())
        .await
        .expect("read-only mount");
    let first = squeezefs::ro_coherence::revalidate_volume(reader.as_ref())
        .await
        .expect("the ledger poll must succeed");
    assert!(
        !first.roots_advanced,
        "nothing was committed: the roots cannot have advanced"
    );

    // The writer commits and checkpoints: the roots move.
    Metadata::create(writer.as_ref(), 1, "new-file", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    writer.checkpoint_now().await.expect("checkpoint");

    let second = squeezefs::ro_coherence::revalidate_volume(reader.as_ref())
        .await
        .expect("the ledger poll must succeed");
    assert!(
        second.roots_advanced,
        "the reader must observe the writer's new checkpoint (epoch {} vs mounted {})",
        second.ledger_seq,
        reader.mounted_ledger().seq
    );
    assert!(second.ledger_seq > first.ledger_seq);

    writer.shutdown().await.unwrap();
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
