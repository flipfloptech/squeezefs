//! PR RW1 disabled-cost pin (docs/design-random-small-writes.md PR RW1;
//! the M2 memoized-gate cost contract, carried to the write rig).
//!
//! This binary NEVER sets `SQUEEZEFS_OP_PROFILE`, so the process-global
//! memoized gate resolves OFF — the production default. Contract:
//!
//! - the write rig records NOTHING (no phase samples, no per-site lock
//!   waits, no stripe-audit classifications, no in-flight samples) on a
//!   real striped write storm — the disabled path takes no stamps;
//! - the rig-off write path is BYTE-IDENTICAL: every storm write reads
//!   back exactly, through the same overlays and spill machinery;
//! - the always-on §1.2 device-byte LEDGER counters still count (they are
//!   plain relaxed counters, deliberately NOT gated — the red gate consumes
//!   them without profile mode);
//! - the disabled stats surface carries the ledger but NOT the rig families
//!   (`fuse_write_phase_ns` etc. exist only when armed — the M2
//!   byte-identical-JSON contract);
//! - the gate stays memoized: arming the env var after first resolution
//!   changes nothing.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    block_lock_site_json, block_lock_stripe_audit_json, op_profile_enabled, write_inflight_json,
    write_profile_phase_json, SqueezefsFilesystem, WriteInflight, METRICS, STATS_INODE,
};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

async fn open_v3_meta(
    path: &std::path::Path,
    len: u64,
) -> std::sync::Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        len,
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
    squeezefs::meta_backend::kv::backend::KvMetaBackend::open(path)
        .await
        .expect("open v3 meta volume")
}

const BS: u64 = 64 * 1024;

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(test_id: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    let dlm = DlmClient::new("local").unwrap();

    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(b.path().to_str().unwrap()));
    let ba = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .unwrap(),
    );
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("32MB"),
        Some("32MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);

    let m = NamedTempFile::new().unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![
        open_v3_meta(m.path(), 128 * 1024 * 1024).await,
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
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

async fn create(h: &H, name: &str) -> u64 {
    h.fs.create(h.req, 1, OsStr::new(name), libc::S_IFREG | 0o644, 0)
        .await
        .unwrap()
        .attr
        .ino
}

async fn write_at(h: &H, ino: u64, off: u64, data: &[u8]) {
    let w =
        h.fs.write(
            h.req,
            ino,
            0,
            off,
            bytes::Bytes::copy_from_slice(data),
            0,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("write ino {ino} off {off} failed: {e:?}"));
    assert_eq!(w.written as usize, data.len(), "short write at {off}");
}

async fn read_at(h: &H, ino: u64, off: u64, size: u32) -> Vec<u8> {
    h.fs.read(h.req, ino, 0, off, size, 0)
        .await
        .unwrap_or_else(|e| panic!("read ino {ino} off {off} failed: {e:?}"))
        .data
        .to_vec()
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 + seed as u64) % 251) as u8)
        .collect()
}

fn all_zero(hist_family: &serde_json::Value) -> bool {
    match hist_family {
        serde_json::Value::Object(map) => map.values().all(all_zero),
        serde_json::Value::Number(n) => n.as_u64() == Some(0),
        _ => false,
    }
}

/// The one big pin: rig OFF ⇒ zero recorded samples + byte-identical data +
/// always-on ledger still counting + rig families absent from `.stats` +
/// the gate memoized.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_rig_disabled_records_nothing_and_path_is_byte_identical() {
    assert!(
        !op_profile_enabled(),
        "SQUEEZEFS_OP_PROFILE unset ⇒ the write rig is OFF by default"
    );

    let h = make("rw1_off").await;
    let ino = create(&h, "off.dat").await;

    // Striped fixture + a partial-write storm exercising checkout / sibling
    // probe / merge / park (12 blocks — no spill needed for the pin).
    let blocks = 12u64;
    let base = pattern((blocks * BS) as usize, 41);
    write_at(&h, ino, 0, &base).await;

    let probes0 = METRICS.staging_sibling_probes.load(Ordering::Relaxed);
    let mut expect = base.clone();
    let payload = pattern(4096, 77);
    for b in 0..blocks {
        let off = b * BS + 8192;
        write_at(&h, ino, off, &payload).await;
        expect[off as usize..off as usize + payload.len()].copy_from_slice(&payload);
    }

    // 1. The rig recorded NOTHING.
    assert!(
        all_zero(&write_profile_phase_json()),
        "disabled rig must record no write-phase samples"
    );
    assert!(
        all_zero(&block_lock_site_json()),
        "disabled rig must record no per-site lock waits"
    );
    let audit = block_lock_stripe_audit_json();
    for key in ["cross_key_waits", "same_key_waits", "waiters_at_arrival"] {
        assert!(
            all_zero(&audit[key]),
            "disabled rig must record no stripe-audit samples ({key})"
        );
    }
    assert_eq!(audit["spill_victim_lock_skips"].as_u64(), Some(0));
    assert!(
        all_zero(&write_inflight_json()),
        "disabled rig must record no in-flight WRITE samples"
    );
    let inflight = WriteInflight::enter();
    drop(inflight);
    assert!(
        all_zero(&write_inflight_json()),
        "WriteInflight::enter is a no-op when the rig is off"
    );

    // 2. The always-on ledger still counts (deliberately NOT gated).
    assert_eq!(
        METRICS.staging_sibling_probes.load(Ordering::Relaxed) - probes0,
        blocks,
        "the H1 sibling-probe ledger counter is always-on (one per striped \
         block write), rig on or off"
    );

    // 3. Byte-identical data through the rig-off path.
    for b in 0..blocks {
        let got = read_at(&h, ino, b * BS, BS as u32).await;
        assert_eq!(
            got,
            expect[(b * BS) as usize..((b + 1) * BS) as usize].to_vec(),
            "block {b}: rig-off write path must be byte-identical"
        );
    }

    // 4. The disabled stats surface: ledger present, rig families ABSENT
    //    (the M2 byte-identical-JSON contract extends to RW1's families).
    let opened =
        h.fs.open(h.req, STATS_INODE, libc::O_RDONLY as u32)
            .await
            .expect("open .stats");
    let data =
        h.fs.read(h.req, STATS_INODE, opened.fh, 0, 16 * 1024 * 1024, 0)
            .await
            .expect("read .stats")
            .data
            .to_vec();
    h.fs.release(h.req, STATS_INODE, opened.fh, 0, 0, false)
        .await
        .expect("release .stats");
    let stats: serde_json::Value = serde_json::from_slice(&data).expect("stats JSON parses");
    let m = &stats["metrics"];
    for absent in [
        "fuse_write_phase_ns",
        "block_lock_wait_by_site",
        "block_lock_stripe_audit",
        "fuse_write_inflight",
    ] {
        assert!(
            m[absent].is_null(),
            "disabled mount's stats surface must NOT carry {absent}"
        );
    }
    for present in [
        "staging_sibling_probes",
        "spill_seed_read_bytes",
        "staging_put_bytes_drain",
        "write_block_revisits",
        "aligned_pool_misses",
    ] {
        assert!(
            !m[present].is_null(),
            "always-on ledger field {present} must be on the stats surface"
        );
    }

    // 5. Memoized: arming the variable after first resolution is inert
    //    (no per-op env reads on the hot path).
    std::env::set_var("SQUEEZEFS_OP_PROFILE", "1");
    assert!(
        !op_profile_enabled(),
        "the gate consulted the environment after first resolution — a \
         per-op env::var read on the hot path"
    );
    std::env::remove_var("SQUEEZEFS_OP_PROFILE");
    let g = WriteInflight::enter();
    drop(g);
    assert!(
        all_zero(&write_inflight_json()),
        "late env arming must not enable the rig (memoized gate)"
    );
}
