//! Write in-handler economy campaign (2026-08-01) — Phase 2 build
//! contracts: the WORK-removal pair named by the closed Phase 1 table
//! (field attribution, `.benchmarks/2026-08-01-write-in-handler.md`):
//! at the EXA write shape the in-handler leg decomposed as admit_gate
//! 3.07 ms/op (conserved queueing — the depth-pin A/B already proved it
//! does not convert) + ~1.5 ms/op of REMOVABLE per-op work, dominated
//! by two structurally futile mechanisms:
//!
//! 1. **The unconditional staged-sibling `spawn_blocking` hop** — one
//!    blocking-pool round trip per striped block write, under the held
//!    block lock, measured 0.45 ms/op mean at saturation with ZERO
//!    siblings ever found (2.14 M probes, `restage_churn_removes` 0).
//!    The occupancy index (`has_staged_active_block`) is latch-free and
//!    conservative-present, and every staging put site for a key holds
//!    that key's block lock — so under the held lock, index-absent is
//!    EXACT: nothing staged, nothing can become staged. The hop is owed
//!    only when the index says present (the RW1 H1 pin anticipated
//!    exactly this flip: "RW3's lock-free probe flips this pin when it
//!    elides the hop").
//! 2. **The cache-less futile spill engine** — on a cache-less volume
//!    (`staging_dirs` empty) `spill_parked_toward_cap` can never reduce
//!    the parked gauge: BOTH spill arms (extent records + full-image
//!    puts) land in staging, and `put_active_block`/`put_extent_record`
//!    refuse unconditionally. The field row measured 980 k refused
//!    staging puts + 1,898 futile 4 MiB seed READS per 70 s — victim
//!    zero-complete/snapshot/spawn_blocking work paid entirely to be
//!    refused, while holding victim block locks against live writers.
//!    The pass short-circuits structurally on cache-less mounts; the
//!    R5 Red machinery (which uploads DURABLY, not to staging) is
//!    untouched.
//!
//! Contracts:
//!
//! 1. **Sibling-hop elision, absent case**: striped block writes with
//!    nothing staged grow `staging_sibling_probes` (the probe is now
//!    latch-free) and the NEW `staging_sibling_hops_elided` gauge in
//!    lockstep; `restage_churn_removes` stays 0.
//! 2. **Sibling-hop fidelity, present case**: a planted staged sibling
//!    is still found and removed at the next write's checkout
//!    (`restage_churn_removes` +1, elided does NOT count it) — the
//!    one-authority law is untouched.
//! 3. **Cache-less spill short-circuit**: on a cache-less fixture with
//!    the parked gauge over cap, the spill pass returns via the NEW
//!    `spill_pass_cacheless_skips` gauge with ZERO futile work
//!    (`spill_staging_puts`, `spill_seed_reads`, `extent_spills` all
//!    unmoved); with staging configured the gauge never moves.
//! 4. Both new gauges ride the stats inode (always-on counters).
//!
//! RED against 514a78e: neither gauge exists; the hop and the futile
//! pass both fire.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS, STATS_INODE};
use squeezefs::routing::DataRouter;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

const FBS: u64 = 4096;

/// Process-global counters; delta tests serialize.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct H {
    fs: Arc<SqueezefsFilesystem>,
    dlm: DlmClient,
    req: Request,
    _backing: NamedTempFile,
    _m: NamedTempFile,
    _s: Option<tempfile::TempDir>,
}

/// Full-FS harness; `staged` = with a staging dir (default venue) or
/// cache-less (`staging_dirs` empty — the field venue).
async fn make_harness(test_id: &str, staged: bool) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FBS.to_string());
    let dlm = DlmClient::new().unwrap();
    let backing = NamedTempFile::new().unwrap();
    std::fs::File::create(backing.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        backing.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(test_id).await.unwrap());
    let (s, dirs) = if staged {
        let d = tempdir().unwrap();
        let p = d.path().to_path_buf();
        (Some(d), vec![p])
    } else {
        (None, vec![])
    };
    let cache = TieredCache::new(
        dirs,
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba.clone(), nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    squeezefs::meta_backend::kv::builder::format_v3(
        m.path(),
        128 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3 meta volume");
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        squeezefs::meta_backend::kv::backend::KvMetaBackend::open(m.path())
            .await
            .expect("open v3 meta volume"),
    ]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
    };
    H {
        fs: Arc::new(fs),
        dlm,
        req,
        _backing: backing,
        _m: m,
        _s: s,
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 7 + seed as u64) % 251) as u8)
        .collect()
}

/// Create a striped fixture file of `blocks` blocks and drain the pipe.
async fn make_striped(h: &H, name: &str, blocks: u64) -> u64 {
    let ino =
        h.fs.create(
            h.req,
            1,
            std::ffi::OsStr::new(name),
            libc::S_IFREG | 0o644,
            0,
        )
        .await
        .expect("create")
        .attr
        .ino;
    let p0 = pattern((blocks * FBS) as usize, 42);
    let w =
        h.fs.write(h.req, ino, 0, 0, bytes::Bytes::copy_from_slice(&p0), 0, 0)
            .await
            .expect("fixture write");
    assert_eq!(w.written as u64, blocks * FBS, "short fixture write");
    h.fs.fsync(h.req, ino, 0, false).await.expect("fsync");
    assert!(
        h.fs.write_pipeline
            .quiesce(std::time::Duration::from_secs(30))
            .await,
        "fixture pipeline must drain"
    );
    ino
}

struct Deltas {
    probes: u64,
    elided: u64,
    churn: u64,
    skips: u64,
    spill_puts: u64,
    spill_seeds: u64,
    extent_spills: u64,
}

fn snap() -> Deltas {
    Deltas {
        probes: METRICS.staging_sibling_probes.load(Ordering::Relaxed),
        elided: METRICS.staging_sibling_hops_elided.load(Ordering::Relaxed),
        churn: METRICS.restage_churn_removes.load(Ordering::Relaxed),
        skips: METRICS.spill_pass_cacheless_skips.load(Ordering::Relaxed),
        spill_puts: METRICS.spill_staging_puts.load(Ordering::Relaxed),
        spill_seeds: METRICS.spill_seed_reads.load(Ordering::Relaxed),
        extent_spills: METRICS.extent_spills.load(Ordering::Relaxed),
    }
}

fn delta(b: &Deltas) -> Deltas {
    let a = snap();
    Deltas {
        probes: a.probes - b.probes,
        elided: a.elided - b.elided,
        churn: a.churn - b.churn,
        skips: a.skips - b.skips,
        spill_puts: a.spill_puts - b.spill_puts,
        spill_seeds: a.spill_seeds - b.spill_seeds,
        extent_spills: a.extent_spills - b.extent_spills,
    }
}

// ---------------------------------------------------------------------------
// Contract 1 — sibling-hop elision on the nothing-staged path.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sibling_hop_elides_when_nothing_is_staged() {
    let _g = serial().await;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let h = make_harness("wih_elide", true).await;
    let ino = make_striped(&h, "elide.dat", 8).await;

    let b0 = snap();
    // One partial (>=25 % — full-buffer checkout, not an extent park)
    // rewrite per block: 8 checkouts, nothing staged anywhere.
    let payload = pattern((FBS / 4) as usize, 5);
    for b in 0..8u64 {
        let w =
            h.fs.write(
                h.req,
                ino,
                0,
                b * FBS + 1024,
                bytes::Bytes::copy_from_slice(&payload),
                0,
                0,
            )
            .await
            .expect("partial write");
        assert_eq!(w.written as u64, FBS / 4);
    }
    let d = delta(&b0);
    assert_eq!(
        d.probes, 8,
        "the probe still fires once per striped block write (now latch-free)"
    );
    assert_eq!(
        d.elided, 8,
        "index-absent checkouts must ELIDE the spawn_blocking hop \
         (got {} of 8)",
        d.elided
    );
    assert_eq!(d.churn, 0, "nothing staged, nothing removed");
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
}

// ---------------------------------------------------------------------------
// Contract 2 — a real staged sibling is still found and removed.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn planted_staged_sibling_is_still_removed_at_checkout() {
    let _g = serial().await;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let h = make_harness("wih_sibling", true).await;
    let ino = make_striped(&h, "sibling.dat", 4).await;

    // Plant a staged sibling for block 2 (an older accumulation image —
    // the restage-churn shape).
    let key = squeezefs::keys::active_block(ino, 2).to_string();
    let token = h.dlm.get_fencing_token_ino(ino);
    let staged = pattern(FBS as usize, 9);
    assert!(
        h.fs.router
            .cache
            .nvme
            .put_active_block(&key, &staged, token),
        "fixture premise: the staging put must be admitted"
    );

    let b0 = snap();
    let payload = pattern((FBS / 4) as usize, 6);
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            2 * FBS + 1024,
            bytes::Bytes::copy_from_slice(&payload),
            0,
            0,
        )
        .await
        .expect("partial write over staged sibling");
    assert_eq!(w.written as u64, FBS / 4);
    let d = delta(&b0);
    assert_eq!(d.probes, 1, "one probe for the one block write");
    assert_eq!(
        d.churn, 1,
        "the planted sibling must be found and removed (one-authority law)"
    );
    assert_eq!(
        d.elided, 0,
        "an index-present checkout must NOT elide the remove hop"
    );
    assert!(
        !h.fs.router.cache.nvme.has_staged_active_block(&key),
        "the staged sibling must be gone after the checkout"
    );
    squeezefs::fuse_client::set_patch_max_bytes(512 * 1024);
}

// ---------------------------------------------------------------------------
// Contract 3 — cache-less spill pass short-circuits with zero futile work.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cacheless_spill_pass_short_circuits_over_cap() {
    let _g = serial().await;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let h = make_harness("wih_cacheless", false).await;
    assert!(
        h.fs.router.cache.nvme.staging_dirs().is_empty(),
        "fixture premise: cache-less"
    );
    let ino = make_striped(&h, "cl.dat", 4).await;

    // Force the over-cap condition: every parked byte exceeds the budget.
    let cap0 = squeezefs::fuse_client::parked_cap_buffers();
    squeezefs::fuse_client::set_parked_cap_buffers(0);

    let b0 = snap();
    // Partial writes park full-repr buffers (>= 25 %) — each op runs the
    // R5 admission pass over cap on a cache-less volume.
    let payload = pattern((FBS / 4) as usize, 7);
    for b in 0..4u64 {
        let w =
            h.fs.write(
                h.req,
                ino,
                0,
                b * FBS + 1024,
                bytes::Bytes::copy_from_slice(&payload),
                0,
                0,
            )
            .await
            .expect("partial write");
        assert_eq!(w.written as u64, FBS / 4);
    }
    let d = delta(&b0);
    squeezefs::fuse_client::set_parked_cap_buffers(cap0);
    assert!(
        d.skips >= 4,
        "every over-cap admission pass on a cache-less volume must \
         short-circuit (spill_pass_cacheless_skips got {}, want >= 4)",
        d.skips
    );
    assert_eq!(
        (d.spill_puts, d.spill_seeds, d.extent_spills),
        (0, 0, 0),
        "the futile spill engine must not run on cache-less \
         (puts/seeds/extent-spills)"
    );
}

// ---------------------------------------------------------------------------
// Contract 3b — with staging configured the skip gauge never moves.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn staged_venue_never_counts_cacheless_skips() {
    let _g = serial().await;
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let h = make_harness("wih_staged_skip", true).await;
    let ino = make_striped(&h, "sk.dat", 4).await;

    let cap0 = squeezefs::fuse_client::parked_cap_buffers();
    squeezefs::fuse_client::set_parked_cap_buffers(0);
    let b0 = snap();
    let payload = pattern((FBS / 4) as usize, 8);
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            1024,
            bytes::Bytes::copy_from_slice(&payload),
            0,
            0,
        )
        .await
        .expect("partial write");
    assert_eq!(w.written as u64, FBS / 4);
    let d = delta(&b0);
    squeezefs::fuse_client::set_parked_cap_buffers(cap0);
    assert_eq!(
        d.skips, 0,
        "a staged venue must never count cache-less skips (the pass runs)"
    );
}

// ---------------------------------------------------------------------------
// Contract 4 — both gauges ride the stats inode, always-on.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn economy_gauges_ride_the_stats_inode() {
    let _g = serial().await;
    let h = make_harness("wih_stats", false).await;
    let reply =
        h.fs.read(h.req, STATS_INODE, 0, 0, 1 << 22, 0)
            .await
            .expect("read stats inode");
    let stats: serde_json::Value =
        serde_json::from_slice(&reply.data).expect("stats inode must be valid JSON");
    let m = stats.get("metrics").expect("metrics object");
    for key in ["staging_sibling_hops_elided", "spill_pass_cacheless_skips"] {
        assert!(
            m.get(key).is_some_and(|v| v.is_u64()),
            "stats inode must carry {key} always-on"
        );
    }
}
