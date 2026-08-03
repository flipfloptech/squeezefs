//! **`offset ‖ incarnation` block keys** — pre-RC engineering spec §6.2
//! **item 6**, whose rationale is §6.3's *block-key binding* paragraph
//! (rulings **D8**/**D9**; design
//! `docs/design-mw-cursors-and-incarnation.md`), behind incompat bit 11,
//! built but **NOT stamped**.
//!
//! The assumption being broken: block keys are bare, reusable device
//! offsets. §6.3 spells out why this is the dangerous one — the read path's
//! serve proof is *"bytes for key K serve for block b iff the fetch was
//! incarnation-valid AND the current map still binds b → K"*, and **both
//! premises are process-local**. The incarnation word returns
//! `UNKNOWN_STABLE` for any offset this node did not itself allocate, and
//! the binding check is deliberately consulted without a TTL gate. So if
//! node A overwrites a block (CoW), frees the offset, and the allocator
//! reissues it to a different file, node B — whose cached map still binds
//! b → K and whose incarnation word is untouched — **serves the other
//! file's bytes with no error and no counter**. On a passthrough volume
//! that is silent; on a transformed volume the AEAD tag fails, which is the
//! one honest degradation.
//!
//! Carrying the lifetime IN the key makes that stale binding
//! **structurally detectable**: a key names not just where but *which
//! lifetime*, so the offset's live lifetime can contradict it.
//!
//! Pinned here: the wire form and its effect on every consumer of the ONE
//! extraction path (`clean_block_key` + `parse_block_key`, which the
//! durable-refcount work shares); how a lifetime advances and why it
//! survives a remount (the durable writer-term era — incompat bit 7 —
//! composed exactly as the DLM's S2 fencing token is); the reclaim
//! window's "offsets stay non-reallocatable until reclaimed" invariant; and
//! byte-identity on an un-stamped volume.
//!
//! NOT built or pinned here: cross-node arbitration. Who may allocate, and
//! the shared authority that would answer "what lifetime does the SET
//! believe this offset is in", is §6.9 **S9**.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::{DlmClient, TERM_MAX};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::journal::AppendPartition;
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_block_key_incarnation_bit, write_superblock_v3, VolumeFormat,
    FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION, FEATURE_INCOMPAT_KV_DURABLE_TERM,
};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{
    block_key_incarnation, block_key_with_incarnation, block_mapping_form, clean_block_key,
    compose_incarnation, decode_incarnation, encode_incarnation, incarnation_era,
    is_whole_block_mapping, BackendRouter, BlockMapOp, DataRouter, LayoutFlip, INCARNATION_NONE,
    INCARNATION_SEQ_BITS,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

const META_LEN: u64 = 256 * 1024 * 1024;
const DATA_LEN: u64 = 2 * 1024 * 1024 * 1024;
const BLOCK: u64 = 4 * 1024 * 1024;
const DATA_VOL_ID: &str = "vol-00000000000000c6";

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn format_meta(path: &std::path::Path) {
    format_v3(path, META_LEN, &opts())
        .await
        .expect("format v3 meta volume");
}

/// [`format_meta`] plus the Phase-8 stamp (bit 11). Fresh formats already
/// carry the durable term (bit 7), which bit 11 REQUIRES.
async fn format_meta_stamped(path: &std::path::Path) {
    format_meta(path).await;
    assert!(
        set_block_key_incarnation_bit(path)
            .await
            .expect("stamp bit 11"),
        "a fresh format must NOT already carry bit 11 — stamping is the \
         Phase-8 window's act, not format's (ruling D9)"
    );
}

fn data_file() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    std::fs::File::create(f.path())
        .unwrap()
        .set_len(DATA_LEN)
        .unwrap();
    f
}

/// A bare router over one file-backed data volume — no metadata plane, for
/// the codec/allocator contracts.
async fn bare_router(data: &std::path::Path) -> (Arc<BlockAllocator>, Arc<NvmeBlockDev>, BackendRouter)
{
    let alloc = Arc::new(BlockAllocator::new(DATA_VOL_ID).await.unwrap());
    alloc.set_capacity_bytes(DATA_LEN);
    let dev = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let router = BackendRouter::new(alloc.clone(), dev.clone(), Arc::new(AtomicU64::new(BLOCK)));
    (alloc, dev, router)
}

/// One mounted harness: a real v3 meta volume, a real file-backed data
/// volume, and the router that binds them (the engagement path runs in
/// `DataRouter::set_meta_backend`).
struct Rig {
    router: DataRouter,
    alloc: Arc<BlockAllocator>,
    dev: Arc<NvmeBlockDev>,
    routed: Arc<RoutedMetaBackend>,
    _staging: TempDir,
}

async fn mount(meta: &std::path::Path, data: &std::path::Path) -> Rig {
    let kv = KvMetaBackend::open(meta).await.expect("open v3 meta volume");
    let routed = Arc::new(RoutedMetaBackend::new(vec![kv]));
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(DATA_VOL_ID).await.unwrap());
    alloc.set_capacity_bytes(DATA_LEN);
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, alloc.clone(), nvme.clone());
    router.set_meta_backend(routed.clone());
    Rig {
        router,
        alloc,
        dev: nvme,
        routed,
        _staging: staging,
    }
}

impl Rig {
    async fn mk_file(&self, name: &str) -> u64 {
        self.routed
            .create(1, name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create")
            .ino
    }

    /// Allocate + write + publish one block and bind it through the shared
    /// merge primitive — the one place a striped block map changes.
    /// Returns `(offset, persisted key)`.
    async fn publish_block(&self, ino: u64, block_index: u32, fill: u8) -> (u64, String) {
        let offset = self.alloc.allocate_block().await.expect("allocate");
        self.dev
            .write_block(offset, bytes::Bytes::from(vec![fill; BLOCK as usize]))
            .await
            .expect("device write");
        self.alloc.publish_block(offset);
        let key = self
            .router
            .backend_router
            .persist_block_key("backend_0", offset);
        self.router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&[(block_index, key.clone())]),
                (block_index as u64 + 1) * BLOCK,
                LayoutFlip::ToStripedKeepStagedIdentity,
                self.router.dlm.get_fencing_token_ino(ino),
            )
            .await
            .expect("merge published block");
        (offset, key)
    }

    async fn shutdown(self) {
        self.routed.volumes[0]
            .shutdown()
            .await
            .expect("clean shutdown");
    }
}

fn refusals() -> u64 {
    squeezefs::fuse_client::METRICS
        .block_key_incarnation_refusals
        .load(Ordering::Relaxed)
}

fn unknowns() -> u64 {
    squeezefs::fuse_client::METRICS
        .block_key_incarnation_unknown
        .load(Ordering::Relaxed)
}

// ===========================================================================
// 1. The wire form: canonical, decoration-compatible, W1-compatible.
// ===========================================================================

/// The codec: canonical lowercase base-36 round-trips, and every
/// non-canonical spelling is REFUSED rather than rounded to a lifetime —
/// two spellings of one lifetime would break the map-binding equality the
/// whole design rests on.
#[test]
fn the_incarnation_codec_is_canonical_and_refuses_junk() {
    for inc in [
        1u64,
        35,
        36,
        1 << 20,
        (7u64 << INCARNATION_SEQ_BITS) | 12345,
        u64::MAX,
    ] {
        let text = encode_incarnation(inc);
        assert_eq!(
            decode_incarnation(&text),
            Some(inc),
            "base-36 must round-trip {inc} (rendered '{text}')"
        );
        assert!(
            !text.starts_with('0'),
            "canonical form has no leading zero: '{text}'"
        );
        assert!(text.len() <= 13, "a u64 fits 13 base-36 digits: '{text}'");
    }
    for junk in ["", "0", "01", "0a", "-1", "AB", "a b", "zzzzzzzzzzzzzz", "1@2"] {
        assert_eq!(
            decode_incarnation(junk),
            None,
            "'{junk}' must be refused, never rounded to a lifetime"
        );
    }
    // Composition refuses anything that would alias a real stamp.
    assert_eq!(compose_incarnation(0, 1), None, "era 0 is not an era");
    assert_eq!(compose_incarnation(1, 0), None, "seq 0 is INCARNATION_NONE");
    assert_eq!(
        compose_incarnation(TERM_MAX + 1, 1),
        None,
        "an era past the term budget would overflow into a foreign era"
    );
    assert_eq!(
        compose_incarnation(1, (1 << INCARNATION_SEQ_BITS)),
        None,
        "a sequence past the budget would alias the next era"
    );
    let stamp = compose_incarnation(9, 42).expect("legal stamp");
    assert_eq!(incarnation_era(stamp), 9, "the era is recoverable — forensics");
}

/// The key's wire form composes with EVERY existing key rule: the
/// `clean_block_key` strip keeps the lifetime (it is part of the key's
/// identity, so all five block-key cache stores and the map-binding check
/// compare lifetimes), the `:rel:len` decoration still parses after it, the
/// `damaged:` quarantine prefix still strips, and the W1 whole-block
/// predicate's polarity is untouched.
#[test]
fn the_wire_form_composes_with_every_existing_key_rule() {
    let inc = compose_incarnation(3, 77).unwrap();
    let text = encode_incarnation(inc);

    for body in ["4194304", "vol-00aa11bb://4194304"] {
        let stamped = block_key_with_incarnation(body, inc);
        assert_eq!(stamped, format!("{body}@{text}"));
        assert_eq!(
            block_key_with_incarnation(body, INCARNATION_NONE),
            body,
            "NONE must return the body verbatim — that is what keeps an un-stamped \
             volume's keys byte-identical"
        );
        assert_eq!(
            clean_block_key(&stamped),
            stamped,
            "the lifetime survives the cleaner: it names WHICH block, not extra data"
        );
        let decorated = format!("{stamped}:0:4194304");
        assert_eq!(
            clean_block_key(&decorated),
            stamped,
            "the decoration still strips to the lifetime-bearing base key"
        );
        assert_eq!(
            clean_block_key(&format!("damaged:{decorated}")),
            stamped,
            "a quarantine marker still strips to its base key"
        );
        assert_eq!(block_key_incarnation(&decorated), Some(inc));
        assert_eq!(
            block_key_incarnation(body),
            Some(INCARNATION_NONE),
            "a legacy key names no lifetime"
        );
        assert_eq!(
            block_key_incarnation(&format!("{body}@!!")),
            None,
            "a malformed suffix is refused, not guessed"
        );
        // W1 predicate polarity (design-random-small-writes §5.1, review
        // Issue 19): a stamped whole-block mapping is STILL the eligible
        // undecorated form, and the decorated one is still ineligible.
        assert!(
            is_whole_block_mapping(&stamped),
            "a stamped whole-block mapping must stay W1-eligible"
        );
        assert!(!is_whole_block_mapping(&decorated));
        assert_eq!(block_mapping_form(&stamped), "undecorated-2part");
        assert_eq!(block_mapping_form(&decorated), "decorated-3part");
    }
}

/// The ONE extraction path stays one path: `parse_block_key` (which the
/// durable-refcount work, fsck, the movers, free and refcount all share)
/// resolves a stamped key to the SAME `(backend, offset)` it always did,
/// and the new parse hands the lifetime back beside it. A malformed
/// lifetime is refused rather than mis-resolved to an offset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_shared_extraction_path_is_not_forked() {
    let data = data_file();
    let (_alloc, _dev, router) = bare_router(data.path()).await;
    let inc = compose_incarnation(2, 5).unwrap();
    let stamped = block_key_with_incarnation("4194304", inc);

    assert_eq!(
        router.parse_block_key(&stamped).unwrap(),
        ("backend_0".to_string(), 4 << 20),
        "every existing consumer must keep resolving the same offset"
    );
    assert_eq!(router.parse_block_offset(&stamped).unwrap(), 4 << 20);
    let parts = router.parse_block_key_parts(&stamped).unwrap();
    assert_eq!((parts.offset, parts.incarnation), (4 << 20, inc));
    let bare = router.parse_block_key_parts("4194304").unwrap();
    assert_eq!(
        (bare.offset, bare.incarnation),
        (4 << 20, INCARNATION_NONE),
        "a legacy key parses as incarnation 0 — an un-stamped volume is unchanged"
    );
    assert!(
        router.parse_block_key("4194304@!!").is_err(),
        "a malformed lifetime must refuse the resolution, never silently drop to the offset"
    );
}

// ===========================================================================
// 2. Minting: one site, era-composed, reallocation-fresh.
// ===========================================================================

/// A disengaged allocator (every volume today) mints NO lifetime and its
/// keys are byte-identical to the shipped form. Engaging it stamps every
/// key it hands out, with the era recoverable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engagement_is_what_stamps_a_key() {
    let data = data_file();
    let (alloc, _dev, router) = bare_router(data.path()).await;

    let off = alloc.allocate_block().await.expect("allocate");
    assert!(!alloc.incarnations_engaged());
    assert_eq!(
        router.persist_block_key("backend_0", off),
        off.to_string(),
        "a disengaged allocator's keys are the shipped bare form, byte for byte"
    );
    assert_eq!(alloc.live_incarnation(off), INCARNATION_NONE);

    router
        .engage_incarnation_keys(11, AppendPartition::SOLO)
        .expect("engage");
    assert!(alloc.incarnations_engaged());
    // The offset allocated BEFORE engagement keeps naming no lifetime —
    // minting is an allocation-time act, never a rewrite of live keys.
    assert_eq!(router.persist_block_key("backend_0", off), off.to_string());

    let fresh = alloc.allocate_block().await.expect("allocate");
    let key = router.persist_block_key("backend_0", fresh);
    let parts = router.parse_block_key_parts(&key).unwrap();
    assert_ne!(parts.incarnation, INCARNATION_NONE, "'{key}' must be stamped");
    assert_eq!(incarnation_era(parts.incarnation), 11, "era 11 composed in");
    assert_eq!(
        router.persist_block_key("backend_0", fresh),
        key,
        "the stamp is READ, not re-minted: two key builds for one offset must agree \
         (a second minting site would make the live map disagree with the live lifetime)"
    );
    assert!(
        router.engage_incarnation_keys(0, AppendPartition::SOLO).is_err(),
        "era 0 must be refused — the era IS the durable writer term"
    );
}

/// The core mechanism: a reallocated offset gets a **fresh** lifetime, so
/// the previous owner's key and the new owner's key are different strings
/// for the same offset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reallocated_offset_names_a_new_lifetime() {
    let data = data_file();
    let (alloc, _dev, router) = bare_router(data.path()).await;
    router
        .engage_incarnation_keys(4, AppendPartition::SOLO)
        .expect("engage");

    let off = alloc.allocate_block().await.expect("allocate");
    let first = router.persist_block_key("backend_0", off);
    assert!(alloc.begin_free(off), "terminal free");
    alloc.finish_free(off);
    let again = alloc.allocate_block().await.expect("reallocate");
    assert_eq!(again, off, "the free list hands the same offset back");
    let second = router.persist_block_key("backend_0", off);
    assert_ne!(
        first, second,
        "two lifetimes of one offset must be two different keys — that IS the detection"
    );
    assert!(
        block_key_incarnation(&second).unwrap() > block_key_incarnation(&first).unwrap(),
        "lifetimes advance monotonically within a mount"
    );
}

/// The reclaim window is untouched: "offsets stay non-reallocatable until
/// reclaimed" is a device-level invariant about shared hardware, and naming
/// lifetimes must not weaken it. Between `begin_free` and `finish_free` the
/// offset is not handed out, and the reclaim path keys on the OFFSET, so a
/// stamped key does not disturb it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reclaim_window_invariant_survives_lifetimes() {
    let data = data_file();
    let (alloc, _dev, router) = bare_router(data.path()).await;
    router
        .engage_incarnation_keys(6, AppendPartition::SOLO)
        .expect("engage");

    let off = alloc.allocate_block().await.expect("allocate");
    let doomed_key = router.persist_block_key("backend_0", off);
    assert!(alloc.begin_free(off), "terminal free");
    // In the window: the offset is the freer's until finish_free.
    let other = alloc.allocate_block().await.expect("allocate");
    assert_ne!(
        other, off,
        "an offset in the begin_free → finish_free window must never be reallocated"
    );
    alloc.finish_free(off);
    let reused = alloc.allocate_block().await.expect("reallocate");
    assert_eq!(reused, off);
    let live_key = router.persist_block_key("backend_0", off);
    assert_ne!(doomed_key, live_key);
    assert!(
        router.block_key_incarnation_ok(&live_key),
        "the live key must validate"
    );
    assert!(
        !router.block_key_incarnation_ok(&doomed_key),
        "the reclaimed lifetime's key must not"
    );
}

// ===========================================================================
// 3. §6.3's failure, made detectable.
// ===========================================================================

/// **The §6.3 scenario, end to end.** A block is written for file A, freed
/// (CoW overwrite / unlink), and the allocator reissues the offset to file
/// B, which writes its own bytes there. A stale binding to A's key then:
///
/// * before item 6 — served B's bytes with **no error and no counter**;
/// * now — is REFUSED, counted on `block_key_incarnation_refusals`, and
///   B's own key still serves B's bytes.
///
/// The same refusal covers the destructive face: a FREE under A's stale key
/// would have released an offset B owns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_binding_is_refused_instead_of_serving_another_files_bytes() {
    let data = data_file();
    let (alloc, dev, router) = bare_router(data.path()).await;
    router
        .engage_incarnation_keys(3, AppendPartition::SOLO)
        .expect("engage");

    // File A's block.
    let off = alloc.allocate_block().await.expect("allocate");
    let key_a = router.persist_block_key("backend_0", off);
    dev.write_block(off, bytes::Bytes::from(vec![0xAAu8; BLOCK as usize]))
        .await
        .expect("write A");
    alloc.publish_block(off);
    let served = router
        .read_block(&key_a, BLOCK as usize)
        .await
        .expect("A's own key serves A's bytes");
    assert_eq!(served[0], 0xAA);

    // A's block is displaced and the offset reissued to B, which writes
    // its own bytes at the same place.
    assert!(alloc.begin_free(off), "terminal free");
    alloc.finish_free(off);
    let reissued = alloc.allocate_block().await.expect("reallocate");
    assert_eq!(reissued, off, "the offset is reused — the §6.3 precondition");
    let key_b = router.persist_block_key("backend_0", off);
    dev.write_block(off, bytes::Bytes::from(vec![0xBBu8; BLOCK as usize]))
        .await
        .expect("write B");
    alloc.publish_block(off);

    let before = refusals();
    let err = router
        .read_block(&key_a, BLOCK as usize)
        .await
        .expect_err("A's stale key must NOT serve B's bytes");
    assert!(
        format!("{err}").contains("dead incarnation"),
        "the refusal must name the cause: {err}"
    );
    assert_eq!(
        refusals(),
        before + 1,
        "the refusal must be COUNTED — the silent case is what §6.3 indicts"
    );
    // A ranged read takes the same path.
    assert!(
        router.read_block_range(&key_a, 0, 4096, None).await.is_err(),
        "the ranged device leg must refuse the dead lifetime too"
    );
    // The destructive face: freeing under the dead key would release B's
    // block (and queue a discard over B's bytes).
    let before = refusals();
    assert!(
        router.free_block(&key_a).await.is_err(),
        "a free under a dead lifetime must be refused"
    );
    assert_eq!(refusals(), before + 1, "and counted");
    assert_eq!(
        alloc.refcount(off),
        Some(1),
        "B's block must still be referenced — the refusal is leak-safe, never a release"
    );

    // B's own key still works, and serves B's bytes.
    let served = router
        .read_block(&key_b, BLOCK as usize)
        .await
        .expect("the live key serves");
    assert_eq!(served[0], 0xBB, "the live lifetime serves its own bytes");
}

/// §6.3's honest degradation, preserved and MEASURED: an offset this node
/// never allocated has no local lifetime answer, so a stamped key naming it
/// is served (exactly as `UNKNOWN_STABLE` behaves today) and counted on
/// `block_key_incarnation_unknown`. That counter is the size of the gap a
/// shared custody authority (§6.9 S9) has to close — it is deliberately not
/// a refusal, because refusing every foreign offset would break the
/// multi-volume read path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_offset_with_no_recorded_lifetime_serves_and_is_counted() {
    let data = data_file();
    let (alloc, dev, router) = bare_router(data.path()).await;
    router
        .engage_incarnation_keys(5, AppendPartition::SOLO)
        .expect("engage");

    // A key from "another node": a lifetime stamp for an offset this
    // allocator never handed out.
    let foreign_off = 128 * BLOCK;
    dev.write_block(
        foreign_off,
        bytes::Bytes::from(vec![0xCCu8; BLOCK as usize]),
    )
    .await
    .expect("write");
    let foreign_key = block_key_with_incarnation(
        &foreign_off.to_string(),
        compose_incarnation(2, 999).unwrap(),
    );
    assert_eq!(alloc.live_incarnation(foreign_off), INCARNATION_NONE);

    let before = (refusals(), unknowns());
    let served = router
        .read_block(&foreign_key, BLOCK as usize)
        .await
        .expect("an unknown lifetime must still serve — inventing an answer would be a lie");
    assert_eq!(served[0], 0xCC);
    assert_eq!(refusals(), before.0, "no refusal for an unknown lifetime");
    assert!(unknowns() > before.1, "the gap must be counted, not hidden");

    // First-touch seeding: once the walk/resolution has recorded the
    // lifetime, the offset presents its real era instead of "unknown".
    alloc.seed_incarnation(foreign_off, compose_incarnation(2, 999).unwrap());
    assert_eq!(
        incarnation_era(alloc.live_incarnation(foreign_off)),
        2,
        "a walked key's era must be recoverable after seeding"
    );
    assert!(router.block_key_incarnation_ok(&foreign_key));
    let stale = block_key_with_incarnation(
        &foreign_off.to_string(),
        compose_incarnation(1, 5).unwrap(),
    );
    assert!(
        !router.block_key_incarnation_ok(&stale),
        "a key from an OLDER era of a seeded offset is now detectable"
    );
}

// ===========================================================================
// 4. Durability: the era ladder is what survives the remount.
// ===========================================================================

/// The lifetime must survive the remount that item 1's ledger survives, or
/// the detection is only intra-mount. It does, without any new durable
/// record: the stamp's high component is the volume's **durable writer
/// term** (incompat bit 7), bumped and barriered before the guard arms, so
/// every stamp a successor mount mints dominates every stamp its
/// predecessor could have written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_durable_era_makes_lifetimes_unrepeatable_across_remounts() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;
    let data = data_file();

    let mut eras = Vec::new();
    let mut keys = Vec::new();
    for _ in 0..3 {
        let rig = mount(meta.path(), data.path()).await;
        assert!(
            rig.routed.volumes[0].block_key_incarnation_engaged(),
            "a stamped volume with a durable term engages incarnation keys at mount"
        );
        assert!(
            rig.alloc.incarnations_engaged(),
            "the mount wiring must engage the data allocator"
        );
        let ino = rig.mk_file(&format!("f{}", eras.len())).await;
        let (_off, key) = rig.publish_block(ino, 0, 0x11).await;
        let inc = block_key_incarnation(&key).unwrap();
        assert_ne!(inc, INCARNATION_NONE, "a published key must be stamped");
        eras.push(incarnation_era(inc));
        keys.push(inc);
        rig.shutdown().await;
    }
    assert!(
        eras.windows(2).all(|w| w[1] > w[0]),
        "each mount's era must dominate its predecessor's: {eras:?}"
    );
    assert!(
        keys.windows(2).all(|w| w[1] > w[0]),
        "and therefore every lifetime it mints: {keys:?}"
    );
}

/// Bit 11 REQUIRES bit 7. Without the durable era the lifetime stamps would
/// restart at every mount, and a stale key would then MATCH the offset's
/// new lifetime — a detection that lies is worse than no detection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stamping_incarnations_without_a_durable_term_is_refused() {
    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path()).await;
    // Strip bit 7 — the pre-S2 shape.
    let VolumeFormat::V3(mut sb) = classify_volume(meta.path()).await.unwrap() else {
        panic!("expected v3");
    };
    sb.features_incompat &= !FEATURE_INCOMPAT_KV_DURABLE_TERM;
    write_superblock_v3(meta.path(), &sb).await.unwrap();

    let err = set_block_key_incarnation_bit(meta.path())
        .await
        .expect_err("bit 11 without bit 7 must be refused");
    let msg = format!("{err}");
    assert!(
        msg.contains("bit 7") && msg.contains("term"),
        "the refusal must name the missing era source: {msg}"
    );

    // And a term-less volume never engages at mount, even if the bit is
    // forced on: `writer_term() == 0` composes no stamp.
    let VolumeFormat::V3(mut sb) = classify_volume(meta.path()).await.unwrap() else {
        panic!("expected v3");
    };
    sb.features_incompat |= FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION;
    write_superblock_v3(meta.path(), &sb).await.unwrap();
    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    assert_eq!(rig.routed.volumes[0].writer_term(), 0);
    assert!(
        !rig.routed.volumes[0].block_key_incarnation_engaged(),
        "no durable era ⇒ no engagement, so keys stay bare rather than lying"
    );
    assert!(!rig.alloc.incarnations_engaged());
    rig.shutdown().await;
}

/// **Ruling D9's boundary.** An un-stamped volume — every volume in the
/// field — must be untouched: no key gains a suffix, nothing engages, and
/// sector 0 is byte-identical after a mount **and a publish**.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unstamped_volume_is_unchanged_by_mount_and_a_publish() {
    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path()).await;
    let before = std::fs::read(meta.path()).unwrap()[..4096].to_vec();

    let data = data_file();
    let rig = mount(meta.path(), data.path()).await;
    assert!(
        !rig.routed.volumes[0].block_key_incarnation_engaged(),
        "an un-stamped volume must not engage incarnation keys"
    );
    assert!(!rig.alloc.incarnations_engaged());

    let ino = rig.mk_file("unstamped").await;
    let (off, key) = rig.publish_block(ino, 0, 0x22).await;
    assert_eq!(
        key,
        off.to_string(),
        "an un-stamped volume's published key is the bare offset, byte for byte"
    );
    assert_eq!(block_key_incarnation(&key), Some(INCARNATION_NONE));
    let served = rig
        .router
        .backend_router
        .read_block(&key, BLOCK as usize)
        .await
        .expect("reads work exactly as before");
    assert_eq!(served[0], 0x22);
    rig.shutdown().await;

    let after = std::fs::read(meta.path()).unwrap()[..4096].to_vec();
    assert_eq!(
        before, after,
        "mounting AND publishing on an un-stamped volume must leave sector 0 \
         byte-identical (the batched reformat window owns the stamp — ruling D9)"
    );
}

/// **The crash leg** (TEST-1's data-device power-cut harness): a power cut
/// loses the volatile writes a barrier never covered, so the bytes behind a
/// pre-cut key are gone — and after the remount the offset is reissued
/// under a NEW era. The pre-cut key must be refused rather than serving
/// whatever survived at that offset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_power_cut_and_remount_refuse_the_pre_cut_lifetime() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_stamped(meta.path()).await;
    let data = data_file();

    squeezefs::dev_power_cut::clear_faults();
    squeezefs::dev_power_cut::arm_power_cut(data.path());

    // Mount 1: publish a block, then lose the volatile cache.
    let (pre_cut_key, offset) = {
        let rig = mount(meta.path(), data.path()).await;
        let ino = rig.mk_file("precut").await;
        let (off, key) = rig.publish_block(ino, 0, 0xA5).await;
        assert!(
            squeezefs::dev_power_cut::volatile_writes(data.path()) > 0,
            "the harness must have journaled the data write (armed at the worker boundary)"
        );
        rig.shutdown().await;
        (key, off)
    };
    let restored = squeezefs::dev_power_cut::power_cut(data.path());
    assert!(
        restored > 0,
        "the cut must have reverted at least one uncovered write"
    );

    // Mount 2: a fresh era, a fresh allocator (a remount's RAM state), and
    // the offset reissued to a different file.
    let rig = mount(meta.path(), data.path()).await;
    let ino = rig.mk_file("postcut").await;
    // A remount starts with fresh allocator RAM, so the freshly-minted
    // first block IS the pre-cut offset — the reissue §6.3 describes.
    let reissued = rig.alloc.allocate_block().await.expect("reallocate");
    assert_eq!(
        reissued, offset,
        "the remounted allocator reissues the same device offset"
    );
    rig.dev
        .write_block(offset, bytes::Bytes::from(vec![0x5Au8; BLOCK as usize]))
        .await
        .expect("post-cut write");
    rig.alloc.publish_block(offset);
    let live_key = rig
        .router
        .backend_router
        .persist_block_key("backend_0", offset);
    assert_ne!(
        live_key, pre_cut_key,
        "the post-crash lifetime must be a different key"
    );
    assert!(
        block_key_incarnation(&live_key).unwrap() > block_key_incarnation(&pre_cut_key).unwrap(),
        "the durable era ladder is what orders lifetimes across a crash"
    );
    let before = refusals();
    assert!(
        rig.router
            .backend_router
            .read_block(&pre_cut_key, BLOCK as usize)
            .await
            .is_err(),
        "the pre-cut key names a dead lifetime and must be refused"
    );
    assert_eq!(refusals(), before + 1);
    let served = rig
        .router
        .backend_router
        .read_block(&live_key, BLOCK as usize)
        .await
        .expect("the live key serves");
    assert_eq!(served[0], 0x5A);
    let _ = ino;
    rig.shutdown().await;
    squeezefs::dev_power_cut::clear_faults();
}
