//! PR K7 scale & gate tests: big directories, the §5.1 readdir cookie
//! contract end-to-end through FUSE, the `dir_entry_cache` ≤ 10 K policy,
//! the builder-built mount-time bound, metadata write amplification, and
//! the per-format stats-JSON scoping (design `docs/design-cow-kv-metadata.md`
//! §5.1, §8 rows 5–7, §10).
//!
//! Contracts pinned:
//! - **§5.1 cookie contract, end-to-end**: synthetic `.`/`..` at offsets
//!   1/2; a real entry's FUSE offset is `3 + ((hash54 << 8) | coll_seq)`
//!   (the dentry key suffix — sign bit provably clear); resume at cookie
//!   `c` returns exactly the strictly-greater suffixes; the root's virtual
//!   `.config`/`.stats` ride above the whole real-cookie space. The forced
//!   `hash54 == 0, coll_seq == 0` name (cookie == 3, the bias boundary) and
//!   the forced top-of-range name are constructed **through the real seeded
//!   hash** by solving the per-volume seed (the xxh3 short-input path is
//!   algebraically invertible — see [`xxh3_seed`]); the solver is verified
//!   against `dentry_name_hash54` itself, so a hash-implementation drift
//!   fails loud here rather than silently mooting the §5.1 edge cases.
//! - **v3 readdir streams**: big directories page through
//!   `readdir(dir, offset, max)` range scans (offset/max honored through
//!   the routed trait), never materialize behind the legacy 100 K cap, and
//!   never populate `dir_entry_cache` / `open_dir_streams`. v2 keeps its
//!   positional-offset behavior byte-for-byte (pinned green here).
//! - **`dir_entry_cache` policy (§4.5)**: only directories ≤ 10 K entries
//!   are cached; larger listings bypass the cache on every format.
//! - **§8 row 6**: the 1 M-entry single-directory trait-path storm —
//!   create / lookup-p50-within-2×-small-dir / streamed readdir / rmdir.
//!   `SQUEEZEFS_SCALE_DIR_ENTRIES` overrides the population for local
//!   iteration; the gate default is the full million.
//! - **§8 row 5**: a 1 M-ino `kv::builder` image (digest-validated method:
//!   builder output equals trait-built state on the same description)
//!   cold-mounts within 500 ms in the serial gate; the 10 M / 100 M
//!   variants are `#[ignore]`d nightly cases driven by
//!   `tests/long_validation.py --mount-scale`.
//! - **§8 row 7**: paired create/unlink storms — v3 device bytes
//!   (`meta_kv_journal_bytes` + node writeback byte counters) vs v2
//!   (`meta_commit_sectors` × 4 KiB, conservatively lower-bounded per
//!   histogram bucket) show ≥ 10× reduction.
//! - **§10 scoping**: `meta_kv_*` stats emitted only when a v3 volume is
//!   mounted; the v2-only trio (`meta_commit_sectors`, `meta_inode_alloc_*`,
//!   `meta_sector_lock_*`) only when a v2 volume is — no misleading zeros.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::{
    SqueezefsFilesystem, CONFIG_INODE, DIR_ENTRY_CACHE_MAX_ENTRIES, METRICS,
    READDIR_VIRTUAL_CONFIG_COOKIE, READDIR_VIRTUAL_STATS_COOKIE, STATS_INODE,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{digest_backend, BuilderConfig, ImageBuilder, ROOT_INO};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::kv::record::{
    decode_readdir_cookie, dentry_key_suffix, dentry_name_hash54, ReaddirPos, HASH54_MAX,
    READDIR_COOKIE_BIAS,
};
use squeezefs::meta_backend::kv::{
    META_KV_JOURNAL_BYTES, META_KV_JOURNAL_ENTRIES, META_KV_NODE_APPENDS,
    META_KV_NODE_APPEND_BYTES, META_KV_NODE_REWRITE_BYTES,
};
use squeezefs::meta_backend::storage::MetaLvStorage;
use squeezefs::meta_backend::{MetaLvBackend, Metadata, RoutedMetaBackend, VolumeBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::DataRouter;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::{tempdir, NamedTempFile};

// ---------------------------------------------------------------------------
// Constants & knobs.
// ---------------------------------------------------------------------------

/// Fixed identity for deterministic v3 images (the kv_backend_tests
/// convention).
const TEST_SEED: u64 = 0x5CA1_AB1E_0DDB_A110;
const TEST_UUID: [u8; 16] = *b"kv-scale-test!!!";

/// v2 volumes need the fixed-geometry floor.
const V2_VOL_LEN: u64 = 128 * 1024 * 1024;

/// §8 row 6 population. Optimized builds (the K7 gate run, nightly,
/// anything `--release`) default to the full million; debug builds
/// default to 100 K — the same storm shape at 10× the cache-policy cap,
/// measured 22 min for the full million at opt-level 0 (2.2 K trait
/// creates/s), which would dwarf the whole per-commit serial gate.
/// `SQUEEZEFS_SCALE_DIR_ENTRIES=1000000` forces the full population
/// anywhere; the K7 closing report records the release-mode million-entry
/// run as the §8 row 6 evidence.
fn scale_dir_entries() -> usize {
    std::env::var("SQUEEZEFS_SCALE_DIR_ENTRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if cfg!(debug_assertions) {
            100_000
        } else {
            1_000_000
        })
}

fn builder_config(node_size: usize, ring: Option<u64>) -> BuilderConfig {
    BuilderConfig {
        node_size,
        journal_len_override: ring,
        hash_seed: TEST_SEED,
        uuid: TEST_UUID,
    }
}

/// An empty v3 volume (root only) with the given seed, mounted read-write.
async fn v3_volume_with_seed(seed: u64, vol_len: u64) -> (Arc<KvMetaBackend>, NamedTempFile) {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(vol_len).unwrap();
    let cfg = BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: seed,
        uuid: TEST_UUID,
    };
    ImageBuilder::new(cfg)
        .unwrap()
        .build(file.path(), vol_len)
        .await
        .expect("build empty v3 image");
    let be = KvMetaBackend::open(file.path()).await.expect("mount v3");
    (be, file)
}

/// A fresh empty v2 volume behind the dispatch. `vol_len` bounds the v2
/// ino cap (`min(20000, (len − 72 MiB) / 32 KiB)`): 128 MiB ⇒ ~1.8 K
/// inos; the > 10 K-dir cache-policy case needs 512 MiB (~14 K inos).
async fn v2_volume(vol_len: u64) -> (Arc<MetaLvBackend>, NamedTempFile) {
    let file = NamedTempFile::new().expect("temp volume");
    let storage = MetaLvStorage::open(file.path(), vol_len).unwrap();
    MetaLvBackend::format_v2_for_tests(&storage, true, true, None)
        .await
        .unwrap();
    (Arc::new(MetaLvBackend::new(storage)), file)
}

// ---------------------------------------------------------------------------
// The seed solver: invert xxh3's short-input (4..=8 B) path so a chosen
// name lands on a chosen 54-bit hash under a *solved* per-volume seed.
// §5.1: "K7 tests force a hash54 == 0, coll_seq == 0 name and a
// top-of-range name" — brute force over 2^54 is not a test strategy; the
// algebra is.
// ---------------------------------------------------------------------------

mod xxh3_seed {
    /// `readLE64(kSecret + 8)` of the reference XXH3 default secret.
    const SECRET_8: u64 = u64::from_le_bytes([0x7c, 0x01, 0x81, 0x2c, 0xf7, 0x21, 0xad, 0x1c]);
    /// `readLE64(kSecret + 16)`.
    const SECRET_16: u64 = u64::from_le_bytes([0xde, 0xd4, 0x6d, 0xe9, 0x83, 0x90, 0x97, 0xdb]);
    /// XXH3's `PRIME_MX2`.
    const PRIME_MX2: u64 = 0x9FB2_1C65_1E98_DF25;

    /// Multiplicative inverse of an odd constant mod 2^64 (Newton).
    fn mul_inv64(a: u64) -> u64 {
        let mut x: u64 = 1;
        for _ in 0..6 {
            x = x.wrapping_mul(2u64.wrapping_sub(a.wrapping_mul(x)));
        }
        x
    }

    /// Inverse of `h ^= h >> 28`.
    fn xorshift28_inv(y: u64) -> u64 {
        y ^ (y >> 28) ^ (y >> 56)
    }

    /// Inverse of `h ^= rotl(h, 49) ^ rotl(h, 24)`: the map is
    /// `M = 1 + x^49 + x^24` over GF(2)[x]/(x^64 − 1); `M^64 = 1`, so the
    /// inverse is `M^63 = M^1·M^2·M^4·M^8·M^16·M^32` — six doubled-rotation
    /// applications.
    fn rot_mix_inv(y: u64) -> u64 {
        let mut h = y;
        let (mut r1, mut r2) = (49u32, 24u32);
        for _ in 0..6 {
            h = h ^ h.rotate_left(r1) ^ h.rotate_left(r2);
            r1 = (r1 * 2) % 64;
            r2 = (r2 * 2) % 64;
        }
        h
    }

    /// Inverse of XXH3's `rrmxmx` finalizer for the 4..=8 B path.
    fn rrmxmx_inv(y: u64, len: u64) -> u64 {
        let inv = mul_inv64(PRIME_MX2);
        let mut h = xorshift28_inv(y);
        h = h.wrapping_mul(inv);
        // Forward: h ^= (h >> 35) + len. `(h >> 35) + len` < 2^30, so bits
        // ≥ 30 pass through — `h >> 35` is recoverable from the image.
        h ^= (h >> 35).wrapping_add(len);
        h = h.wrapping_mul(inv);
        rot_mix_inv(h)
    }

    /// Solve the seed making `xxh3_64_with_seed(name, seed) == target`
    /// for a 4..=8 byte name. The forward path is
    /// `seed' = seed ^ (swap32(seed_lo) << 32)`;
    /// `keyed = input64 ^ ((S8 ^ S16) − seed')`; `hash = rrmxmx(keyed, len)`.
    pub fn seed_forcing_full_hash(name: &[u8], target: u64) -> u64 {
        assert!(
            (4..=8).contains(&name.len()),
            "solver covers xxh3's 4..=8 byte path; got {} bytes",
            name.len()
        );
        let input1 = u32::from_le_bytes(name[0..4].try_into().unwrap()) as u64;
        let input2 = u32::from_le_bytes(name[name.len() - 4..].try_into().unwrap()) as u64;
        let input64 = input2.wrapping_add(input1 << 32);
        let keyed = rrmxmx_inv(target, name.len() as u64);
        let bitflip = keyed ^ input64;
        let seed_prime = (SECRET_8 ^ SECRET_16).wrapping_sub(bitflip);
        let lo = seed_prime as u32;
        let hi = ((seed_prime >> 32) as u32) ^ lo.swap_bytes();
        (u64::from(hi) << 32) | u64::from(lo)
    }

    /// Solve the seed forcing `dentry_name_hash54(name, seed) == hash54`
    /// (full hash = `hash54 << 10`; the dropped low 10 bits are free).
    pub fn seed_forcing_hash54(name: &str, hash54: u64) -> u64 {
        seed_forcing_full_hash(name.as_bytes(), hash54 << 10)
    }
}

#[test]
fn seed_solver_forces_bottom_and_top_hash54() {
    // Bottom of the cookie space: hash54 == 0 ⇒ cookie == 3 (== the bias).
    let lo_seed = xxh3_seed::seed_forcing_hash54("cookie-l", 0);
    assert_eq!(
        dentry_name_hash54(b"cookie-l", lo_seed),
        0,
        "solved seed must force hash54 == 0 through the real seeded hash"
    );
    // Top of the range: hash54 == 2^54 − 1.
    let hi_seed = xxh3_seed::seed_forcing_hash54("cookie-h", HASH54_MAX);
    assert_eq!(
        dentry_name_hash54(b"cookie-h", hi_seed),
        HASH54_MAX,
        "solved seed must force the top-of-range hash54"
    );
    // The §5.1 cookie edges those two names exercise.
    assert_eq!(READDIR_COOKIE_BIAS + dentry_key_suffix(0, 0), 3);
    let top_cookie = READDIR_COOKIE_BIAS + dentry_key_suffix(HASH54_MAX, u8::MAX);
    assert_eq!(top_cookie, (1u64 << 62) + 2, "top real cookie = 2^62 + 2");
    assert_eq!(
        top_cookie & (1 << 63),
        0,
        "the sign bit is clear across the whole real-cookie space (§5.1)"
    );
    assert!(matches!(
        decode_readdir_cookie(3),
        Ok(ReaddirPos::AfterEntry {
            hash54: 0,
            coll_seq: 0
        })
    ));
    // Arbitrary mid-range targets solve too (the solver is general).
    for target in [1u64, 42, 1 << 30, HASH54_MAX - 1] {
        let seed = xxh3_seed::seed_forcing_hash54("any-name", target);
        assert_eq!(dentry_name_hash54(b"any-name", seed), target);
    }
}

// ---------------------------------------------------------------------------
// FUSE harness (the meta_lv_fuse_tests shape).
// ---------------------------------------------------------------------------

struct FuseHarness {
    fs: SqueezefsFilesystem,
    _backing: NamedTempFile,
    _staging: tempfile::TempDir,
}

async fn fuse_fs(routed: Arc<RoutedMetaBackend>, test_id: &str) -> FuseHarness {
    let dlm = DlmClient::new("local").unwrap();
    let backing = NamedTempFile::new().unwrap();
    backing.as_file().set_len(16 * 1024 * 1024).unwrap();
    let nvme_dev = Arc::new(NvmeBlockDev::new(backing.path().to_str().unwrap()));
    let block_alloc = Arc::new(
        BlockAllocator::new(dlm.meta_client().clone(), test_id)
            .await
            .expect("BlockAllocator"),
    );
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("16MB"),
        Some("16MB"),
        Some("64MB"),
        Some("64MB"),
        dlm.meta_client().clone(),
        block_alloc.clone(),
        nvme_dev.clone(),
    )
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, block_alloc, nvme_dev);
    let mut fs = SqueezefsFilesystem::new(router, dlm, 1000, 1000);
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    FuseHarness {
        fs,
        _backing: backing,
        _staging: staging,
    }
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 4242,
    }
}

/// Walk a directory through `SqueezefsFilesystem::readdir` the way the
/// kernel does: re-enter at the last consumed entry's offset until an
/// empty batch. `take_per_call` simulates a small kernel buffer (consume
/// only a prefix of each batch — the mid-stream-resume shape).
async fn walk_fuse_readdir(
    fs: &SqueezefsFilesystem,
    parent: u64,
    fh: u64,
    take_per_call: usize,
) -> Vec<(String, u64, i64)> {
    use futures::StreamExt;
    let mut out: Vec<(String, u64, i64)> = Vec::new();
    let mut offset = 0i64;
    let mut calls = 0usize;
    loop {
        calls += 1;
        assert!(
            calls < 1_000_000,
            "readdir walk did not terminate (offset {offset})"
        );
        let reply = fs
            .readdir(req(), parent, fh, offset)
            .await
            .expect("readdir");
        let batch: Vec<_> = reply.entries.collect().await;
        if batch.is_empty() {
            break;
        }
        let take = take_per_call.min(batch.len());
        for entry in batch.into_iter().take(take) {
            let e = entry.expect("dir entry");
            out.push((e.name.to_string_lossy().into_owned(), e.inode, e.offset));
        }
        let last = out.last().expect("consumed at least one").2;
        assert!(
            last > offset,
            "offsets must advance strictly (resume {offset} → last {last})"
        );
        offset = last;
    }
    out
}

/// Same walk through `readdirplus`.
async fn walk_fuse_readdirplus(
    fs: &SqueezefsFilesystem,
    parent: u64,
    take_per_call: usize,
) -> Vec<(String, u64, i64)> {
    use futures::StreamExt;
    let mut out: Vec<(String, u64, i64)> = Vec::new();
    let mut offset = 0u64;
    loop {
        let reply = fs
            .readdirplus(req(), parent, 0, offset, 0)
            .await
            .expect("readdirplus");
        let batch: Vec<_> = reply.entries.collect().await;
        if batch.is_empty() {
            break;
        }
        let take = take_per_call.min(batch.len());
        for entry in batch.into_iter().take(take) {
            let e = entry.expect("dir entry plus");
            out.push((e.name.to_string_lossy().into_owned(), e.inode, e.offset));
        }
        let last = out.last().expect("consumed at least one").2;
        assert!(last as u64 > offset, "offsets must advance strictly");
        offset = last as u64;
    }
    out
}

// ---------------------------------------------------------------------------
// §5.1 cookie contract — trait level (routed dispatch honors offset/max).
// ---------------------------------------------------------------------------

/// The routed `Metadata::readdir` must honor `offset`/`max` on v3 (§5.1:
/// "readdir starts honoring its offset/max parameters on v3") — pages of
/// at most `max`, resumable by the returned key cookies, enumerating
/// exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routed_readdir_pages_honor_offset_and_max_on_v3() {
    let (be, _file) = v3_volume_with_seed(TEST_SEED, 64 * 1024 * 1024).await;
    let routed = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        be.clone(),
    )]));
    let dir = routed
        .create(ROOT_INO, "paged", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let mut expect = HashSet::new();
    for i in 0..97u32 {
        let name = format!("entry-{i:03}");
        routed
            .create(dir.ino, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        expect.insert(name);
    }

    // Page through the trait with max = 10: every page ≤ 10 entries, the
    // resume cookie is the last entry's key suffix + bias, and the union
    // is exactly the population.
    let mut seen = HashSet::new();
    let mut cookie = 0u64;
    let mut pages = 0usize;
    loop {
        let page = routed.readdir(dir.ino, cookie, 10).await.unwrap();
        if page.is_empty() {
            break;
        }
        pages += 1;
        assert!(
            page.len() <= 10,
            "v3 routed readdir must honor max (got a {}-entry page)",
            page.len()
        );
        for d in &page {
            assert!(
                seen.insert(d.name.clone()),
                "entry {} emitted twice across pages",
                d.name
            );
        }
        // §5.1 resume rule: the cookie of the page's last entry.
        let last = page.last().unwrap();
        let h = dentry_name_hash54(last.name.as_bytes(), TEST_SEED);
        cookie = READDIR_COOKIE_BIAS + dentry_key_suffix(h, 0);
    }
    assert!(pages >= 10, "97 entries at max=10 must take ≥ 10 pages");
    assert_eq!(seen, expect, "paged union must be exactly the population");

    be.shutdown().await.unwrap();
}

/// Forced `hash54 == 0, coll_seq == 0` (§5.1): the cookie is exactly the
/// bias (3); a resume at 3 must return every *other* entry exactly once
/// and never re-emit the forced name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_hash54_zero_cookie_resumes_exactly_once() {
    const FORCED: &str = "cookie-l";
    let seed = xxh3_seed::seed_forcing_hash54(FORCED, 0);
    let (be, _file) = v3_volume_with_seed(seed, 64 * 1024 * 1024).await;
    let routed = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        be.clone(),
    )]));

    let dir = routed
        .create(ROOT_INO, "forced-lo", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    routed
        .create(dir.ino, FORCED, libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let mut others = HashSet::new();
    for i in 0..31u32 {
        let name = format!("plain-{i:02}");
        routed
            .create(dir.ino, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        others.insert(name);
    }
    assert_eq!(dentry_name_hash54(FORCED.as_bytes(), seed), 0);

    // The forced entry is the FIRST in key order and its cookie is the
    // bias boundary itself.
    let first = routed.readdir(dir.ino, 0, 1).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0].name, FORCED,
        "hash54 == 0 must sort first in the dentry keyspace"
    );

    // Resume AT cookie 3: strictly-greater suffixes only — never the
    // forced entry again, all others exactly once.
    let rest = routed.readdir(dir.ino, 3, usize::MAX).await.unwrap();
    let rest_names: HashSet<String> = rest.iter().map(|d| d.name.clone()).collect();
    assert_eq!(
        rest.len(),
        others.len(),
        "resume at the bias cookie must return exactly the other entries"
    );
    assert_eq!(rest_names, others);
    assert!(
        !rest_names.contains(FORCED),
        "the hash54==0 entry must not be re-emitted at its own cookie"
    );

    be.shutdown().await.unwrap();
}

/// Forced top-of-range `hash54` (§5.1): the entry sorts last, its cookie
/// sits at the top of the 62-bit payload space with the sign bit clear,
/// and resuming at (or above) it terminates cleanly — the
/// `key_successor` overflow edge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_top_of_range_cookie_is_terminal() {
    const FORCED: &str = "cookie-h";
    let seed = xxh3_seed::seed_forcing_hash54(FORCED, HASH54_MAX);
    let (be, _file) = v3_volume_with_seed(seed, 64 * 1024 * 1024).await;
    let routed = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        be.clone(),
    )]));

    let dir = routed
        .create(ROOT_INO, "forced-hi", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    routed
        .create(dir.ino, FORCED, libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    for i in 0..15u32 {
        routed
            .create(
                dir.ino,
                &format!("plain-{i:02}"),
                libc::S_IFREG | 0o644,
                0,
                0,
            )
            .await
            .unwrap();
    }
    assert_eq!(dentry_name_hash54(FORCED.as_bytes(), seed), HASH54_MAX);

    let all = routed.readdir(dir.ino, 0, usize::MAX).await.unwrap();
    assert_eq!(all.len(), 16);
    assert_eq!(
        all.last().unwrap().name,
        FORCED,
        "the top-of-range hash must sort last in key order"
    );

    let forced_cookie = READDIR_COOKIE_BIAS + dentry_key_suffix(HASH54_MAX, 0);
    assert_eq!(forced_cookie & (1 << 63), 0, "sign bit clear (§5.1)");
    // Resume AT the forced cookie: nothing follows it.
    let after = routed
        .readdir(dir.ino, forced_cookie, usize::MAX)
        .await
        .unwrap();
    assert!(
        after.is_empty(),
        "resume at the top-of-range cookie must be terminal, got {after:?}"
    );
    // Resume at the absolute top of the payload space: the
    // key_successor(all-0xFF suffix) edge must terminate, not wrap or err.
    let top = READDIR_COOKIE_BIAS + dentry_key_suffix(HASH54_MAX, u8::MAX);
    let beyond = routed.readdir(dir.ino, top, usize::MAX).await.unwrap();
    assert!(beyond.is_empty());

    be.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// §5.1 cookie contract — end-to-end through FUSE.
// ---------------------------------------------------------------------------

/// v3 readdir through the FUSE layer: `.`/`..` at offsets 1/2, every real
/// entry at its key cookie (computed independently from the seeded hash),
/// root virtuals `.config`/`.stats` ABOVE the real-cookie space, exact
/// once-each enumeration under mid-stream resume, and no
/// `dir_entry_cache` / `open_dir_streams` population (v3 streams).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fuse_readdir_v3_emits_key_cookies_and_streams() {
    let (be, _file) = v3_volume_with_seed(TEST_SEED, 64 * 1024 * 1024).await;
    let routed = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        be.clone(),
    )]));
    let h = fuse_fs(routed.clone(), "kv_scale_fuse_v3").await;

    let mut expect = HashSet::new();
    for i in 0..41u32 {
        let name = format!("root-entry-{i:02}");
        routed
            .create(ROOT_INO, &name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .unwrap();
        expect.insert(name);
    }

    // Small-buffer walk (take 7 per call) — the resume-heavy shape.
    let walked = walk_fuse_readdir(&h.fs, 1, 0, 7).await;

    // '.' and '..' first, at the reserved offsets.
    assert_eq!(walked[0].0, ".");
    assert_eq!(walked[0].2, 1);
    assert_eq!(walked[1].0, "..");
    assert_eq!(walked[1].2, 2);

    // Real entries: exactly once each, at exactly their key cookies, in
    // strictly ascending cookie order.
    let reals: Vec<&(String, u64, i64)> = walked
        .iter()
        .filter(|(n, _, _)| !matches!(n.as_str(), "." | ".." | ".config" | ".stats"))
        .collect();
    let real_names: HashSet<String> = reals.iter().map(|(n, _, _)| n.clone()).collect();
    assert_eq!(real_names, expect, "every real entry exactly once");
    assert_eq!(reals.len(), expect.len(), "no duplicates");
    for (name, _ino, off) in &reals {
        let hash = dentry_name_hash54(name.as_bytes(), TEST_SEED);
        let want = READDIR_COOKIE_BIAS + dentry_key_suffix(hash, 0);
        assert_eq!(
            *off as u64, want,
            "entry {name} must carry its §5.1 key cookie (got {off}, want {want})"
        );
        assert!(*off > 0, "sign bit / negativity check");
    }

    // Root virtuals ride ABOVE the real cookie space, in order, last.
    let n = walked.len();
    assert_eq!(walked[n - 2].0, ".config");
    assert_eq!(walked[n - 2].1, CONFIG_INODE);
    assert_eq!(walked[n - 2].2 as u64, READDIR_VIRTUAL_CONFIG_COOKIE);
    assert_eq!(walked[n - 1].0, ".stats");
    assert_eq!(walked[n - 1].1, STATS_INODE);
    assert_eq!(walked[n - 1].2 as u64, READDIR_VIRTUAL_STATS_COOKIE);
    assert!(
        READDIR_VIRTUAL_CONFIG_COOKIE
            > READDIR_COOKIE_BIAS + dentry_key_suffix(HASH54_MAX, u8::MAX)
    );
    assert_eq!(READDIR_VIRTUAL_STATS_COOKIE & (1 << 63), 0);

    // v3 must not populate the FUSE-layer whole-dir caches (streaming).
    h.fs.dir_entry_cache.run_pending_tasks();
    assert!(
        h.fs.dir_entry_cache.get(&1).is_none(),
        "v3 readdir must not populate dir_entry_cache"
    );

    // opendir on v3 must not materialize a whole-dir snapshot either.
    let opened = h.fs.opendir(req(), 1, 0).await.expect("opendir");
    assert!(
        h.fs.open_dir_streams.get(&opened.fh).is_none(),
        "v3 opendir must not snapshot the directory into open_dir_streams"
    );
    // ... and readdir through that fh still streams correctly.
    let walked_fh = walk_fuse_readdir(&h.fs, 1, opened.fh, 1000).await;
    assert_eq!(
        walked_fh
            .iter()
            .map(|(n, _, _)| n.clone())
            .collect::<HashSet<_>>(),
        walked
            .iter()
            .map(|(n, _, _)| n.clone())
            .collect::<HashSet<_>>(),
        "fh-path walk must agree with the fh-less walk"
    );
    h.fs.releasedir(req(), 1, opened.fh, 0).await.unwrap();

    be.shutdown().await.unwrap();
}

/// readdirplus on v3: the same cookie contract, with attributes served
/// per entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fuse_readdirplus_v3_streams_with_attrs() {
    let (be, _file) = v3_volume_with_seed(TEST_SEED, 64 * 1024 * 1024).await;
    let routed = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        be.clone(),
    )]));
    let h = fuse_fs(routed.clone(), "kv_scale_fuse_v3_plus").await;

    let dir = routed
        .create(ROOT_INO, "plusdir", libc::S_IFDIR | 0o750, 1000, 1000)
        .await
        .unwrap();
    let mut expect = HashSet::new();
    for i in 0..23u32 {
        let name = format!("pe-{i:02}");
        routed
            .create(dir.ino, &name, libc::S_IFREG | 0o640, 1000, 1000)
            .await
            .unwrap();
        expect.insert(name);
    }

    let walked = walk_fuse_readdirplus(&h.fs, dir.ino, 5).await;
    assert_eq!(walked[0].0, ".");
    assert_eq!(walked[1].0, "..");
    let reals: Vec<_> = walked
        .iter()
        .filter(|(n, _, _)| n != "." && n != "..")
        .collect();
    assert_eq!(
        reals
            .iter()
            .map(|(n, _, _)| n.clone())
            .collect::<HashSet<_>>(),
        expect
    );
    for (name, _, off) in &reals {
        let hash = dentry_name_hash54(name.as_bytes(), TEST_SEED);
        assert_eq!(
            *off as u64,
            READDIR_COOKIE_BIAS + dentry_key_suffix(hash, 0),
            "readdirplus offset for {name} must be the §5.1 cookie"
        );
    }
    // Non-root directory: no virtual entries.
    assert!(walked
        .iter()
        .all(|(n, _, _)| n != ".config" && n != ".stats"));

    be.shutdown().await.unwrap();
}

/// v2 through the same FUSE surface keeps its positional-offset behavior
/// (offsets 1, 2, 3, … with the root virtuals FIRST) and keeps populating
/// `dir_entry_cache` for small directories — the v2-unchanged pin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fuse_readdir_v2_positional_offsets_unchanged() {
    let (be, _file) = v2_volume(V2_VOL_LEN).await;
    let routed = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V2(be)]));
    let h = fuse_fs(routed.clone(), "kv_scale_fuse_v2").await;

    let mut expect = HashSet::new();
    for i in 0..9u32 {
        let name = format!("v2-entry-{i}");
        routed
            .create(ROOT_INO, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        expect.insert(name);
    }

    let walked = walk_fuse_readdir(&h.fs, 1, 0, 4).await;
    // Positional: 1, 2, 3, … in emission order, virtuals before reals.
    for (i, (_, _, off)) in walked.iter().enumerate() {
        assert_eq!(
            *off,
            i as i64 + 1,
            "v2 offsets stay positional (entry {i} at {off})"
        );
    }
    assert_eq!(walked[0].0, ".");
    assert_eq!(walked[1].0, "..");
    assert_eq!(walked[2].0, ".config", "v2 root virtuals stay first");
    assert_eq!(walked[3].0, ".stats");
    let reals: HashSet<String> = walked[4..].iter().map(|(n, _, _)| n.clone()).collect();
    assert_eq!(reals, expect);

    // Small v2 directory: the fh-less fallback path still caches.
    h.fs.dir_entry_cache.run_pending_tasks();
    assert!(
        h.fs.dir_entry_cache.get(&1).is_some(),
        "small v2 directories keep using dir_entry_cache"
    );
}

// ---------------------------------------------------------------------------
// dir_entry_cache policy (§4.5): ≤ 10 K entries cached, larger bypassed.
// ---------------------------------------------------------------------------

/// The cache policy is format-generic: a > 10 K-entry v2 directory must
/// bypass `dir_entry_cache` (an `Arc<[…]>` of a huge listing is exactly
/// the OOM the §4.5 policy exists to prevent), while ≤ 10 K keeps caching.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dir_entry_cache_caps_at_10k_entries() {
    assert_eq!(
        DIR_ENTRY_CACHE_MAX_ENTRIES, 10_000,
        "the §4.5 policy constant"
    );
    let (be, _file) = v2_volume(512 * 1024 * 1024).await;
    let routed = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V2(be)]));
    let h = fuse_fs(routed.clone(), "kv_scale_cache_policy").await;

    // A > 10 K-entry v2 directory (concurrent creates — v2's shared-parent
    // path keeps this fast enough for the serial gate).
    let dir = routed
        .create(ROOT_INO, "big-v2", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let total = DIR_ENTRY_CACHE_MAX_ENTRIES + 50;
    let mut tasks = Vec::new();
    for t in 0..5usize {
        let r = routed.clone();
        let dir_ino = dir.ino;
        tasks.push(tokio::spawn(async move {
            let per = total / 5 + usize::from(t < total % 5);
            for i in 0..per {
                r.create(
                    dir_ino,
                    &format!("f-{t}-{i:05}"),
                    libc::S_IFREG | 0o644,
                    0,
                    0,
                )
                .await
                .expect("create");
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    let walked = walk_fuse_readdir(&h.fs, dir.ino, 0, usize::MAX).await;
    assert_eq!(walked.len(), total + 2, "all entries + . + ..");
    h.fs.dir_entry_cache.run_pending_tasks();
    assert!(
        h.fs.dir_entry_cache.get(&dir.ino).is_none(),
        "a {total}-entry directory must bypass dir_entry_cache (≤ 10 K policy)"
    );

    // A small sibling still caches through the same path.
    let small = routed
        .create(ROOT_INO, "small-v2", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    for i in 0..12u32 {
        routed
            .create(small.ino, &format!("s-{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    let _ = walk_fuse_readdir(&h.fs, small.ino, 0, usize::MAX).await;
    h.fs.dir_entry_cache.run_pending_tasks();
    assert!(
        h.fs.dir_entry_cache.get(&small.ino).is_some(),
        "≤ 10 K directories keep caching"
    );
}

/// A v3 directory larger than the legacy 100 K materialization cap must
/// enumerate completely through FUSE (the pre-K7 snapshot path silently
/// truncated at 100 000 entries) — and stream: no whole-dir cache entries,
/// resume from an arbitrary mid-directory cookie returns exactly the
/// strictly-after set.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fuse_readdir_v3_big_dir_streams_exactly_once() {
    let (be, _file) = v3_volume_with_seed(TEST_SEED, 256 * 1024 * 1024).await;
    let routed = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        be.clone(),
    )]));
    let h = fuse_fs(routed.clone(), "kv_scale_fuse_v3_big").await;

    let dir = routed
        .create(ROOT_INO, "bigdir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let total = DIR_ENTRY_CACHE_MAX_ENTRIES + 2_000; // > the cache policy cap
    let mut tasks = Vec::new();
    for t in 0..6usize {
        let r = routed.clone();
        let dir_ino = dir.ino;
        tasks.push(tokio::spawn(async move {
            let per = total / 6 + usize::from(t < total % 6);
            for i in 0..per {
                r.create(
                    dir_ino,
                    &format!("e-{t}-{i:05}"),
                    libc::S_IFREG | 0o644,
                    0,
                    0,
                )
                .await
                .expect("create");
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    let walked = walk_fuse_readdir(&h.fs, dir.ino, 0, usize::MAX).await;
    let names: HashSet<String> = walked
        .iter()
        .filter(|(n, _, _)| n != "." && n != "..")
        .map(|(n, _, _)| n.clone())
        .collect();
    assert_eq!(
        names.len(),
        total,
        "every entry of a {total}-entry v3 dir must stream through FUSE exactly once"
    );
    h.fs.dir_entry_cache.run_pending_tasks();
    assert!(
        h.fs.dir_entry_cache.get(&dir.ino).is_none(),
        "big v3 directories must never enter dir_entry_cache"
    );

    // Mid-directory cookie resume: pick the walked entry at the 60th
    // percentile; a fresh walk from its cookie must return exactly the
    // entries whose cookies are strictly greater.
    let reals: Vec<&(String, u64, i64)> = walked
        .iter()
        .filter(|(n, _, _)| n != "." && n != "..")
        .collect();
    let pivot = reals[reals.len() * 6 / 10];
    let expected_after: HashSet<String> = reals
        .iter()
        .filter(|(_, _, off)| *off > pivot.2)
        .map(|(n, _, _)| n.clone())
        .collect();
    let mut resumed = HashSet::new();
    let mut offset = pivot.2;
    loop {
        use futures::StreamExt;
        let reply =
            h.fs.readdir(req(), dir.ino, 0, offset)
                .await
                .expect("readdir resume");
        let batch: Vec<_> = reply.entries.collect().await;
        if batch.is_empty() {
            break;
        }
        for e in batch {
            let e = e.unwrap();
            offset = e.offset;
            resumed.insert(e.name.to_string_lossy().into_owned());
        }
    }
    assert_eq!(
        resumed, expected_after,
        "mid-directory resume must return exactly the strictly-after set"
    );

    be.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// §8 row 6: the 1 M-entry single-directory storm (trait path).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn million_entry_directory_storm_create_lookup_readdir_rmdir() {
    let entries = scale_dir_entries();
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(4 * 1024 * 1024 * 1024).unwrap();
    ImageBuilder::new(builder_config(DEFAULT_NODE_SIZE, None))
        .unwrap()
        .build(file.path(), 4 * 1024 * 1024 * 1024)
        .await
        .expect("build v3 image");
    let be = KvMetaBackend::open(file.path()).await.expect("mount");
    let routed = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        be.clone(),
    )]));

    // A small reference directory for the lookup-p50 comparison.
    let small = routed
        .create(ROOT_INO, "small", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    for i in 0..100u32 {
        routed
            .create(small.ino, &format!("s{i:03}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }

    let dir = routed
        .create(ROOT_INO, "bigdir", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();

    // ---- Create storm.
    let t0 = Instant::now();
    let writers = 8usize;
    let mut tasks = Vec::new();
    for w in 0..writers {
        let r = routed.clone();
        let dir_ino = dir.ino;
        tasks.push(tokio::spawn(async move {
            let mut i = w;
            while i < entries {
                r.create(dir_ino, &format!("f{i:07}"), libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .expect("storm create");
                i += writers;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let create_elapsed = t0.elapsed();
    eprintln!(
        "[scale] {} creates in one directory: {:.2?} ({:.0} creates/s)",
        entries,
        create_elapsed,
        entries as f64 / create_elapsed.as_secs_f64()
    );

    // ---- Lookup p50: big dir within 2× small dir (§8 row 6).
    let p50 = |mut samples: Vec<Duration>| -> Duration {
        samples.sort_unstable();
        samples[samples.len() / 2]
    };
    let mut small_samples = Vec::with_capacity(2000);
    for i in 0..2000u64 {
        let name = format!("s{:03}", i % 100);
        let t = Instant::now();
        routed.lookup(small.ino, &name).await.expect("small lookup");
        small_samples.push(t.elapsed());
    }
    let mut big_samples = Vec::with_capacity(2000);
    let stride = (entries as u64 / 2000).max(1);
    for i in 0..2000u64 {
        let name = format!("f{:07}", (i * stride) % entries as u64);
        let t = Instant::now();
        routed.lookup(dir.ino, &name).await.expect("big lookup");
        big_samples.push(t.elapsed());
    }
    let (small_p50, big_p50) = (p50(small_samples), p50(big_samples));
    eprintln!("[scale] lookup p50: small-dir {small_p50:.2?}, {entries}-entry dir {big_p50:.2?}");
    assert!(
        big_p50 <= small_p50 * 2,
        "big-dir lookup p50 ({big_p50:?}) must stay within 2× the small-dir p50 ({small_p50:?}) — §8 row 6"
    );

    // ---- Streamed readdir: pages of 10 K, cookie-resumed, exactly once.
    let t1 = Instant::now();
    let mut seen = 0usize;
    let mut inos = HashSet::with_capacity(entries);
    let mut cookie = 0u64;
    loop {
        let page = routed.readdir(dir.ino, cookie, 10_000).await.unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 10_000, "readdir must honor max");
        seen += page.len();
        for d in &page {
            assert!(inos.insert(d.ino), "child ino {} emitted twice", d.ino);
        }
        let last = page.last().unwrap();
        let h = dentry_name_hash54(last.name.as_bytes(), TEST_SEED);
        cookie = READDIR_COOKIE_BIAS + dentry_key_suffix(h, 0);
    }
    let readdir_elapsed = t1.elapsed();
    assert_eq!(
        seen, entries,
        "streamed readdir must enumerate exactly once"
    );
    eprintln!(
        "[scale] streamed readdir of {} entries: {:.2?} ({:.0} entries/s)",
        entries,
        readdir_elapsed,
        entries as f64 / readdir_elapsed.as_secs_f64()
    );

    // ---- Unlink storm + rmdir (the FUSE rmdir shape: emptiness check,
    // unlink, destroy deferred to forget — destroy_inode here).
    let t2 = Instant::now();
    let mut tasks = Vec::new();
    for w in 0..writers {
        let r = routed.clone();
        let dir_ino = dir.ino;
        tasks.push(tokio::spawn(async move {
            let mut i = w;
            while i < entries {
                r.unlink(dir_ino, &format!("f{i:07}"))
                    .await
                    .expect("storm unlink");
                i += writers;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let unlink_elapsed = t2.elapsed();
    eprintln!(
        "[scale] {} unlinks: {:.2?} ({:.0} unlinks/s)",
        entries,
        unlink_elapsed,
        entries as f64 / unlink_elapsed.as_secs_f64()
    );

    let leftover = routed.readdir(dir.ino, 0, 100).await.unwrap();
    assert!(
        leftover.is_empty(),
        "directory must be empty after the storm"
    );
    routed.unlink(ROOT_INO, "bigdir").await.expect("rmdir");
    routed.destroy_inode(dir.ino).await.expect("destroy dir");
    assert!(
        routed.lookup(ROOT_INO, "bigdir").await.is_err(),
        "rmdir must remove the directory"
    );

    be.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// §8 row 5: builder-built mount time — digest-validated method + the
// 1 M-ino serial-gate bound. 10 M / 100 M live in the nightly cases below.
// ---------------------------------------------------------------------------

/// The §8 method's validation clause: the offline builder produces the
/// same post-fold live state as the same population built through the
/// mutating trait — so the mount-time gate below measures real images,
/// not a fiction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_image_digest_matches_trait_built_state() {
    const FILES: u32 = 200;
    let times = |i: u32| (1_000 + i as u64, 2_000 + i as u64, 3_000 + i as u64);

    // Builder side.
    let img_file = NamedTempFile::new().unwrap();
    img_file.as_file().set_len(64 * 1024 * 1024).unwrap();
    let mut b = ImageBuilder::new(builder_config(DEFAULT_NODE_SIZE, None)).unwrap();
    let mut described = Vec::new();
    for i in 0..FILES {
        let ino = b
            .add_file(
                ROOT_INO,
                &format!("df{i:04}"),
                0o644,
                1000,
                1000,
                u64::from(i) * 7,
            )
            .unwrap();
        let (a, m, c) = times(i);
        b.set_times(ino, a, m, c).unwrap();
        described.push(ino);
    }
    b.set_xattr(described[3], "user.tag", b"builder-vs-trait")
        .unwrap();
    b.add_link(described[5], ROOT_INO, "hard.lnk").unwrap();
    b.set_times(ROOT_INO, 0, 0, 0).unwrap();
    b.build(img_file.path(), 64 * 1024 * 1024).await.unwrap();
    let built = KvMetaBackend::open(img_file.path()).await.unwrap();
    let built_digest = digest_backend(&built).await.unwrap();
    built.shutdown().await.unwrap();

    // Trait side: same population through the mutating surface on an
    // empty image with the same seed, times pinned last (creates stamp
    // real clocks; the description's times are explicit).
    let (be, _file) = v3_volume_with_seed(TEST_SEED, 64 * 1024 * 1024).await;
    let routed = RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(be.clone())]);
    let mut trait_inos = Vec::new();
    for i in 0..FILES {
        let ino = routed
            .create(
                ROOT_INO,
                &format!("df{i:04}"),
                libc::S_IFREG | 0o644,
                1000,
                1000,
            )
            .await
            .unwrap()
            .ino;
        trait_inos.push(ino);
    }
    routed
        .setxattr(trait_inos[3], "user.tag", b"builder-vs-trait")
        .await
        .unwrap();
    routed
        .link(trait_inos[5], ROOT_INO, "hard.lnk")
        .await
        .unwrap();
    for (i, ino) in trait_inos.iter().enumerate() {
        let (a, m, c) = times(i as u32);
        routed
            .setattr(
                *ino,
                None,
                None,
                None,
                Some(i as u64 * 7),
                Some(a),
                Some(m),
                Some(c),
            )
            .await
            .unwrap();
    }
    routed
        .setattr(ROOT_INO, None, None, None, None, Some(0), Some(0), Some(0))
        .await
        .unwrap();
    let trait_digest = digest_backend(&be).await.unwrap();
    assert_eq!(
        built_digest, trait_digest,
        "builder images must be digest-identical to trait-built state (§8 method)"
    );
    be.shutdown().await.unwrap();
}

/// Build a `total_inos` gate volume (1 000 files per directory) at `path`.
/// Returns the built-image summary. The shape matches the §3 sizing math:
/// dirs and their children cluster in the ino keyspace.
async fn build_scale_image(path: &std::path::Path, total_inos: u64) -> u64 {
    let files_per_dir = 1_000u64;
    let dirs = total_inos / (files_per_dir + 1) + 1;
    let mut b = ImageBuilder::new(builder_config(DEFAULT_NODE_SIZE, None)).unwrap();
    let mut count = 1u64; // root
    'outer: for d in 0..dirs {
        let dir = b
            .add_dir(ROOT_INO, &format!("d{d:06}"), 0o755, 1000, 1000)
            .unwrap();
        count += 1;
        if count >= total_inos {
            break;
        }
        for f in 0..files_per_dir {
            b.add_file(dir, &format!("f{f:06}"), 0o644, 1000, 1000, 4096)
                .unwrap();
            count += 1;
            if count >= total_inos {
                break 'outer;
            }
        }
    }
    assert_eq!(b.inode_count(), total_inos);
    // Volume: §3 on-disk math (~200 B/object ÷ 0.75) + fixed structures,
    // padded generously; the file is sparse but must span the heap so
    // whole-extent node reads never come up short.
    let volume_len = (total_inos * 400)
        .max(256 * 1024 * 1024)
        .next_multiple_of(4096);
    let f = std::fs::File::create(path).expect("create volume file");
    f.set_len(volume_len).expect("size volume file");
    drop(f);
    let img = b.build(path, volume_len).await.expect("build image");
    assert_eq!(img.next_ino, total_inos + 1);
    img.nodes_written
}

/// Drop the volume's page cache (best effort without root) so the timed
/// mount is cold: fdatasync'd file + `posix_fadvise(DONTNEED)`.
fn drop_page_cache(path: &std::path::Path) {
    use std::os::fd::AsRawFd;
    let f = std::fs::File::open(path).expect("open volume for fadvise");
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

/// §8 row 5, the serial-gate half: a 1 M-ino builder image cold-mounts in
/// ≤ 500 ms. Mount = SB + ledger + bitmap + (empty) journal — O(active
/// set), no table scans; the bound holds with room even unoptimized.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn million_ino_builder_mount_within_bound() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("kv_scale_1m_ino.img");
    let _cleanup = scopeguard_rm(&path);

    let t_build = Instant::now();
    let nodes = build_scale_image(&path, 1_000_000).await;
    eprintln!(
        "[scale] 1M-ino image: {} nodes in {:.2?}",
        nodes,
        t_build.elapsed()
    );

    drop_page_cache(&path);
    let t_mount = Instant::now();
    let be = KvMetaBackend::open(&path).await.expect("cold mount");
    let mount_ms = t_mount.elapsed().as_millis();
    eprintln!("[scale] 1M-ino cold mount: {mount_ms} ms");

    // Sanity: the mount serves reads (root + a deep file).
    assert_eq!(be.next_ino(), 1_000_001);
    let d0 = be.lookup(ROOT_INO, "d000000").await.expect("dir lookup");
    assert!(be.lookup(d0.ino, "f000000").await.is_ok());

    assert!(
        mount_ms <= 500,
        "1M-ino cold mount took {mount_ms} ms — the §8 row 5 serial-gate bound is 500 ms"
    );
    be.shutdown().await.unwrap();
}

/// RAII file removal (scale images are hundreds of MB to tens of GB).
fn scopeguard_rm(path: &std::path::Path) -> impl Drop {
    struct Rm(std::path::PathBuf);
    impl Drop for Rm {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    Rm(path.to_path_buf())
}

/// Nightly (§8 row 5): 10 M-ino builder image cold mount. Invoked by
/// `tests/long_validation.py --mount-scale 10m` (or `both`); see the
/// module docs there for the standalone command.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "nightly — run via tests/long_validation.py --mount-scale 10m"]
async fn nightly_mount_time_10m_ino() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("kv_scale_10m_ino.img");
    let _cleanup = scopeguard_rm(&path);

    let t_build = Instant::now();
    let nodes = build_scale_image(&path, 10_000_000).await;
    eprintln!(
        "[scale] 10M-ino image: {} nodes in {:.2?}",
        nodes,
        t_build.elapsed()
    );
    drop_page_cache(&path);
    let t_mount = Instant::now();
    let be = KvMetaBackend::open(&path).await.expect("cold mount");
    let mount_ms = t_mount.elapsed().as_millis();
    eprintln!("[scale] 10M-ino cold mount: {mount_ms} ms");
    assert_eq!(be.next_ino(), 10_000_001);
    assert!(
        mount_ms <= 2_000,
        "10M-ino cold mount took {mount_ms} ms (pathological bound 2 s)"
    );
    be.shutdown().await.unwrap();
}

/// Nightly (§8 row 5): the 100 M-ino cold mount — ≤ 2 s hard bound,
/// ≤ ~300 ms typical target. Invoked by
/// `tests/long_validation.py --mount-scale 100m` (or `both`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "nightly — run via tests/long_validation.py --mount-scale 100m"]
async fn nightly_mount_time_100m_ino() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("kv_scale_100m_ino.img");
    let _cleanup = scopeguard_rm(&path);

    let t_build = Instant::now();
    let nodes = build_scale_image(&path, 100_000_000).await;
    eprintln!(
        "[scale] 100M-ino image: {} nodes in {:.2?}",
        nodes,
        t_build.elapsed()
    );
    drop_page_cache(&path);
    let t_mount = Instant::now();
    let be = KvMetaBackend::open(&path).await.expect("cold mount");
    let mount_ms = t_mount.elapsed().as_millis();
    eprintln!("[scale] 100M-ino cold mount: {mount_ms} ms (target ≤ 300 ms typical)");
    assert_eq!(be.next_ino(), 100_000_001);
    assert!(
        mount_ms <= 2_000,
        "100M-ino cold mount took {mount_ms} ms — the §8 hard bound is 2 s"
    );
    be.shutdown().await.unwrap();
}

/// §8 micro-gate regression (found by the K7 criterion run): a
/// create/unlink storm leaves the dentry leaf a tombstone desert until
/// compaction folds it, and the chain-window scans behind EVERY
/// lookup/create/unlink (`find_dentry` / `dentry_insert_key` — a ≤ 256-key
/// window) must not fold-walk past their window's end into that desert.
/// Before the fix a post-storm lookup cost ~1000× a pre-storm one
/// (323 ns → 339 µs release — the §8 `lookup_file ≤ v2 + 10 %` gate is
/// unmeetable); bounded scans keep the ratio at ~1×. The 10× assertion
/// margin absorbs machine noise while staying two orders below the bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lookup_p50_immune_to_tombstone_desert() {
    const PAIRS: usize = 4_096;
    let (be, _f) = v3_volume_with_seed(TEST_SEED, 256 * 1024 * 1024).await;

    be.create(ROOT_INO, "live_target", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let p50 = |mut s: Vec<Duration>| {
        s.sort_unstable();
        s[s.len() / 2]
    };
    let sample = |n: usize| {
        let be = be.clone();
        async move {
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                let t = Instant::now();
                be.lookup(ROOT_INO, "live_target").await.unwrap();
                out.push(t.elapsed());
            }
            out
        }
    };
    // Warm-up + baseline.
    let _ = sample(200).await;
    let before = p50(sample(1000).await);

    // The desert: create/unlink pairs sharing the root dentry leaf.
    for i in 0..PAIRS {
        let name = format!("desert{i:05}");
        be.create(ROOT_INO, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        be.unlink(ROOT_INO, &name).await.unwrap();
    }

    let after = p50(sample(1000).await);
    eprintln!(
        "[desert] lookup p50 before {before:.2?} → after {PAIRS}-pair storm {after:.2?} \
         ({}x)",
        after.as_nanos() / before.as_nanos().max(1)
    );
    assert!(
        after <= before.saturating_mul(10) + Duration::from_micros(20),
        "chain-window scans must stay bounded through a tombstone desert: \
         p50 {before:.2?} → {after:.2?} after {PAIRS} create/unlink pairs"
    );
    be.shutdown().await.unwrap();
}

/// §4.6 pt 1 names TWO writeback triggers: "every flush tick, **or when a
/// node's dirty delta exceeds a bset worth**". The threshold half must
/// drain without waiting out the cadence — commits enqueue maintenance
/// when a leaf's open delta crosses `DEFAULT_WRITEBACK_DELTA_BYTES`, and
/// the checkpoint task must wake and append promptly (RAM-apply cost per
/// commit is O(open delta): letting it balloon for a whole tick is the
/// +18 % create-row cliff the K7 §8 criterion run caught). A
/// maintenance-only wake must NOT barrier or checkpoint — those stay on
/// the cadence (the deferred-durability contract is untouched).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn threshold_writeback_drains_without_cadence() {
    // Park the cadence out of reach so only the threshold path can act.
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let _cleanup = Cleanup;

    let (be, _f) = v3_volume_with_seed(TEST_SEED, 128 * 1024 * 1024).await;
    let routed = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        be.clone(),
    )]));

    let appends0 = META_KV_NODE_APPENDS.load(Ordering::Relaxed);
    let checkpoints0 = squeezefs::meta_backend::kv::META_KV_CHECKPOINTS.load(Ordering::Relaxed);
    // ~500 creates ⇒ tens of KiB of dentry+inode+Δtime records — far past
    // one 4 KiB bset worth on both hot leaves.
    for i in 0..500u32 {
        routed
            .create(ROOT_INO, &format!("thr{i:04}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    // The threshold trigger must land appends without a cadence tick
    // (parked at 60 s). Bounded poll — event-driven in the product, the
    // poll is only the observer.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut appends_now = META_KV_NODE_APPENDS.load(Ordering::Relaxed);
    while appends_now == appends0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        appends_now = META_KV_NODE_APPENDS.load(Ordering::Relaxed);
    }
    assert!(
        appends_now > appends0,
        "threshold-crossed dirty deltas must be appended without waiting out the \
         60 s cadence (§4.6 pt 1's second trigger) — appends stayed {appends0}"
    );
    assert_eq!(
        squeezefs::meta_backend::kv::META_KV_CHECKPOINTS.load(Ordering::Relaxed),
        checkpoints0,
        "a maintenance-only wake must not checkpoint (barriers/ledger stay on cadence)"
    );
    be.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// §8 paired-run tooling: rebuild a CLI-formatted meta volume as v2.
// ---------------------------------------------------------------------------

/// Utility for the §8 paired mount benches (not a test): the CLI formats
/// only v3 from K6a on (resolved OQ 4 — v2 formatting is
/// test-surface-only), but every §8 gate row is a paired v2-vs-v3
/// comparison over REAL CLI mounts. This target rebuilds the meta volume
/// at `SQUEEZEFS_V2_REFORMAT_META` as v2 with the §6.2 test-scoped
/// formatter, carrying over the format-config xattr the CLI recorded —
/// the exact shim shape the pre-K6b transport suite used. Invocation:
///
/// ```text
/// SQUEEZEFS_V2_REFORMAT_META=/path/to/meta.bin \
///   cargo test --release --test kv_scale_tests -- --ignored \
///   --exact util_reformat_meta_volume_v2 --test-threads=1
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "paired-bench tooling — set SQUEEZEFS_V2_REFORMAT_META and run explicitly"]
async fn util_reformat_meta_volume_v2() {
    use squeezefs::meta_backend::kv::builder::FORMAT_CONFIG_XATTR;
    let meta = std::env::var("SQUEEZEFS_V2_REFORMAT_META")
        .expect("set SQUEEZEFS_V2_REFORMAT_META to the CLI-formatted meta volume path");
    let len = std::env::var("SQUEEZEFS_V2_REFORMAT_LEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256 * 1024 * 1024);
    let meta = std::path::PathBuf::from(meta);

    let cfg = KvMetaBackend::open(&meta)
        .await
        .expect("open the CLI-formatted v3 meta volume")
        .getxattr(ROOT_INO, FORMAT_CONFIG_XATTR)
        .await
        .expect("read the recorded format config")
        .expect("format must record the config xattr");
    let storage = MetaLvStorage::open(&meta, len).expect("reopen meta volume");
    MetaLvBackend::format_v2_for_tests(&storage, true, true, None)
        .await
        .expect("v2 reformat");
    squeezefs::meta_backend::xattr::set_xattr(&storage, 1, FORMAT_CONFIG_XATTR, &cfg)
        .await
        .expect("carry the format config onto the v2 volume");
    eprintln!(
        "[v2-reformat] {} is now format v2 ({} B)",
        meta.display(),
        len
    );
}

// ---------------------------------------------------------------------------
// §8 row 7: metadata write amplification — paired create/unlink storms.
// ---------------------------------------------------------------------------

/// Conservative lower bound of total sectors recorded in the
/// `meta_commit_sectors` histogram delta (each bucket counted at its
/// smallest representable depth — understating v2 makes the ≥ 10× gate
/// strictly harder to pass).
fn sectors_lower_bound(before: &[u64; 15], after: &[u64; 15]) -> u64 {
    const LOWER: [u64; 15] = [
        0, 1, 2, 3, 5, 9, 17, 33, 65, 129, 257, 513, 1025, 2049, 4097,
    ];
    let mut total = 0u64;
    for i in 0..15 {
        total += (after[i] - before[i]) * LOWER[i];
    }
    total
}

fn commit_sector_buckets() -> [u64; 15] {
    let mut out = [0u64; 15];
    for (i, b) in METRICS.meta_commit_sectors.buckets.iter().enumerate() {
        out[i] = b.load(Ordering::Relaxed);
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn write_amp_create_unlink_storm_v3_ge_10x_reduction() {
    const OPS: usize = 4_096;
    /// A storm, not a trickle: the §4.4 write-amp arithmetic (and the
    /// §8 row 7 gate built on it) is about storm-rate commits, where a
    /// flush tick's 4 KiB bset frame amortizes over the tick's whole
    /// record batch — exactly the shared-parent concurrency the routed
    /// create path is built for (§4.4 pt 6). A serial await-loop pays a
    /// full frame per handful of records and measures the harness's own
    /// slowness, not the design.
    const WRITERS: usize = 8;

    // ---- v3 storm: device bytes = journal entry bytes + node writeback
    // (bset appends + CoW rewrites), §8 row 7's stated accounting.
    let (v3, _f3) = v3_volume_with_seed(TEST_SEED, 256 * 1024 * 1024).await;
    let routed3 = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        v3.clone(),
    )]));
    let j0 = META_KV_JOURNAL_BYTES.load(Ordering::Relaxed);
    let e0 = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed);
    let a0 = META_KV_NODE_APPEND_BYTES.load(Ordering::Relaxed);
    let r0 = META_KV_NODE_REWRITE_BYTES.load(Ordering::Relaxed);
    let n0 = META_KV_NODE_APPENDS.load(Ordering::Relaxed);
    let mut tasks = Vec::new();
    for w in 0..WRITERS {
        let r = routed3.clone();
        tasks.push(tokio::spawn(async move {
            let mut i = w;
            while i < OPS {
                let name = format!("wa{i:05}");
                r.create(ROOT_INO, &name, libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .expect("storm create");
                r.unlink(ROOT_INO, &name).await.expect("storm unlink");
                i += WRITERS;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    // Shutdown runs the final checkpoint: every dirty bset lands, so the
    // append/rewrite deltas are complete, not clocked mid-cadence.
    v3.shutdown().await.unwrap();
    let v3_journal = META_KV_JOURNAL_BYTES.load(Ordering::Relaxed) - j0;
    let v3_entries = META_KV_JOURNAL_ENTRIES.load(Ordering::Relaxed) - e0;
    let v3_appends = META_KV_NODE_APPENDS.load(Ordering::Relaxed) - n0;
    let v3_append_bytes = META_KV_NODE_APPEND_BYTES.load(Ordering::Relaxed) - a0;
    let v3_rewrite_bytes = META_KV_NODE_REWRITE_BYTES.load(Ordering::Relaxed) - r0;
    let v3_bytes = v3_journal + v3_append_bytes + v3_rewrite_bytes;
    assert!(
        v3_entries >= (2 * OPS) as u64,
        "every create/unlink must journal one whole-tx entry (got {v3_entries})"
    );
    assert!(v3_journal > 0 && v3_bytes > 0, "v3 byte counters must move");

    // ---- v2 storm: the same op mix at the same concurrency through the
    // v2 trait surface (its per-commit sector cost is rate-independent,
    // so symmetry here only removes doubt, it does not tilt the gate).
    let (v2, _f2) = v2_volume(512 * 1024 * 1024).await;
    let s0 = commit_sector_buckets();
    let mut tasks = Vec::new();
    for w in 0..WRITERS {
        let v2 = v2.clone();
        tasks.push(tokio::spawn(async move {
            let mut i = w;
            while i < OPS {
                let name = format!("wa{i:05}");
                v2.create(1, &name, libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .expect("v2 storm create");
                v2.unlink(1, &name).await.expect("v2 storm unlink");
                i += WRITERS;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let s1 = commit_sector_buckets();
    let v2_sectors = sectors_lower_bound(&s0, &s1);
    let v2_bytes = v2_sectors * 4096;
    assert!(v2_sectors > 0, "v2 storm must record commit sectors");

    let ratio = v2_bytes as f64 / v3_bytes as f64;
    eprintln!(
        "[write-amp] {OPS} create/unlink pairs — v2: {v2_bytes} B ({v2_sectors} sectors, \
         lower-bounded), v3: {v3_bytes} B (journal {v3_journal} B / {v3_entries} entries, \
         appends {v3_appends} = {v3_append_bytes} B, rewrites {v3_rewrite_bytes} B) ⇒ \
         reduction {ratio:.1}×"
    );
    assert!(
        ratio >= 10.0,
        "§8 row 7: metadata write amplification must fall ≥ 10× (measured {ratio:.2}×; \
         v2 {v2_bytes} B vs v3 {v3_bytes} B)"
    );
}

// ---------------------------------------------------------------------------
// §10: per-format stats scoping on the stats-inode JSON.
// ---------------------------------------------------------------------------

const V2_ONLY_KEYS: [&str; 5] = [
    "meta_commit_sectors",
    "meta_inode_alloc_cas_retries",
    "meta_inode_alloc_reconciled",
    "meta_sector_lock_wait_ns",
    "meta_sector_lock_contended",
];
const V3_KEYS: [&str; 12] = [
    "meta_kv_node_cache_hits",
    "meta_kv_node_cache_misses",
    "meta_kv_node_cache_evictions",
    "meta_kv_node_appends",
    "meta_kv_node_compactions",
    "meta_kv_node_splits",
    "meta_kv_journal_bytes",
    "meta_kv_journal_entries",
    "meta_kv_journal_full_stalls",
    "meta_kv_checkpoints",
    "meta_kv_replay_entries",
    "meta_kv_free_extents",
];

async fn stats_metrics_object(
    fs: &SqueezefsFilesystem,
) -> serde_json::Map<String, serde_json::Value> {
    let json = fs.generate_stats_json().await;
    let v: serde_json::Value = serde_json::from_str(&json).expect("stats JSON parses");
    v.get("metrics")
        .and_then(|m| m.as_object())
        .expect("stats JSON carries a metrics object")
        .clone()
}

/// A v3-only mount emits the `meta_kv_*` family and none of the v2-only
/// counters; a v2-only mount emits the inverse; a mixed set emits both
/// (design §10: "emitted only for v2 volumes … don't emit misleading
/// zeros").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_json_scopes_v2_and_v3_metrics_per_volume() {
    // v3-only.
    let (v3, _f3) = v3_volume_with_seed(TEST_SEED, 64 * 1024 * 1024).await;
    let routed = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V3(
        v3.clone(),
    )]));
    let h = fuse_fs(routed, "kv_scale_stats_v3").await;
    let m = stats_metrics_object(&h.fs).await;
    for key in V3_KEYS {
        assert!(m.contains_key(key), "v3 mount must emit {key}");
    }
    for key in V2_ONLY_KEYS {
        assert!(
            !m.contains_key(key),
            "v3-only mount must NOT emit the v2-only {key} (misleading zeros)"
        );
    }
    assert_eq!(
        m.get("meta_format_version").and_then(|v| v.as_array()),
        Some(&vec![serde_json::Value::String("3".into())]),
    );
    v3.shutdown().await.unwrap();

    // v2-only.
    let (v2be, _f2) = v2_volume(V2_VOL_LEN).await;
    let routed2 = Arc::new(RoutedMetaBackend::new_dispatch(vec![VolumeBackend::V2(
        v2be,
    )]));
    let h2 = fuse_fs(routed2, "kv_scale_stats_v2").await;
    let m2 = stats_metrics_object(&h2.fs).await;
    for key in V2_ONLY_KEYS {
        assert!(m2.contains_key(key), "v2 mount must keep emitting {key}");
    }
    for key in V3_KEYS {
        assert!(
            !m2.contains_key(key),
            "v2-only mount must NOT emit {key} (misleading zeros)"
        );
    }

    // Mixed set: both families.
    let (v3m, _f3m) = v3_volume_with_seed(TEST_SEED, 64 * 1024 * 1024).await;
    let (v2m, _f2m) = v2_volume(V2_VOL_LEN).await;
    let routed_mixed = Arc::new(RoutedMetaBackend::new_dispatch(vec![
        VolumeBackend::V2(v2m),
        VolumeBackend::V3(v3m.clone()),
    ]));
    let hm = fuse_fs(routed_mixed, "kv_scale_stats_mixed").await;
    let mm = stats_metrics_object(&hm.fs).await;
    for key in V2_ONLY_KEYS.iter().chain(V3_KEYS.iter()) {
        assert!(mm.contains_key(*key), "mixed mount must emit {key}");
    }
    v3m.shutdown().await.unwrap();
}
