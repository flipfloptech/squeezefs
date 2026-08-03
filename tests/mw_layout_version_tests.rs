//! Pre-RC engineering spec §6.2 **item 9** — the **durable per-ino
//! layout version** (the last open item on the multi-writer format
//! board; incompat bit 15, `KV_LAYOUT_VERSIONS`, ruling D9: built,
//! never stamped by a production format).
//!
//! The assumption being retired: layout-delta chains name their base
//! with a PROCESS-LOCAL token (`CachedMetadata::layout_base_token` —
//! rewrite-publish-drain Lever A, 2026-08-01). The token restarts with
//! the process and its meaning is anchored in RAM, so under a single
//! writer the era check catches staleness — but an authority failover
//! folds chains whose base was named by a dead process, and S8/S9 ship
//! publishes from co-writers whose notion of the base must agree with
//! the authority's. "Divergent chains fold to divergent layouts" is
//! silent data corruption, not a leak.
//!
//! The contract under test:
//!
//! 1. **The base name is durable and era-composed** — a versioned delta
//!    record carries `(base_version, version)`; `version` is minted by
//!    `crate::dlm::mint_layout_version()` (`(term << 40) | seq`, the
//!    fencing-token composition: monotone, remount-safe, unique across
//!    writer eras).
//! 2. **The commit gate refuses divergence loud** — a delta claiming a
//!    nonzero base that is NOT the durable chain head is REFUSED (an
//!    error naming §6.2 item 9), never staged, never silently folded.
//!    A claim of `0` ("unknown provenance" — a refetched writer) or a
//!    claim against a bare/unversioned head re-bases with the
//!    always-correct full `Put` (the first-touch rule) — convergent by
//!    construction, an `Ok(false)`, never an error.
//! 3. **The fold refuses divergence loud** — a durable chain whose
//!    versioned links do not join (or that mixes versioned and
//!    unversioned links in one base segment) folds to `Corrupt`, not to
//!    a layout nobody computed.
//! 4. **Failover is the acceptance shape** — a successor authority
//!    (term bumped, fresh process state simulated by reopen) folding a
//!    predecessor's chain produces EXACTLY the layout the predecessor
//!    would have, across both the clean-shutdown (bset) and
//!    drop-without-shutdown (journal replay) faces.
//! 5. **The economy is untouched** — versioning adds ZERO journal
//!    entries (the D4 law) and exactly 16 bytes per delta record on a
//!    stamped volume; an UN-stamped volume's wire stays byte-identical
//!    to the shipped bit-5 wire (versions stripped at encode).
//! 6. **Lever A keeps serving** — on a versions-stamped volume the
//!    publish base-provenance ledger still closes against the pass
//!    count and the delta path stays engaged (the 7.7× publish-tax fix
//!    must not regress).
//!
//! RED at the test commit: the scaffold carries the fields but the
//! wire never encodes them, no gate exists, and the fold is
//! version-blind.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::{adopt_durable_term, durable_term, mint_layout_version, DlmClient};
use squeezefs::fuse_client::{SqueezefsFilesystem, METRICS};
use squeezefs::layout_wire::{layout_delta_versions, LayoutDelta, LayoutMetadata};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::record::{
    fold_newest_first, xattr_key, Folded, Record, RecordKind, XattrValue,
};
use squeezefs::meta_backend::kv::superblock::{
    set_layout_versions_bit, SuperblockV3, FEATURES_INCOMPAT_KNOWN,
    FEATURE_INCOMPAT_KV_LAYOUT_VERSIONS, SUPERBLOCK_V3_LEN,
};
use squeezefs::meta_backend::kv::{META_KV_LAYOUT_DELTA_COMMITS, META_KV_LAYOUT_FULL_COMMITS};
use squeezefs::meta_backend::{open_routed_meta_set, Metadata, RoutedMetaBackend};
use squeezefs::routing::DataRouter;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use tempfile::{tempdir, NamedTempFile, TempDir};

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

const VOL_LEN: u64 = 256 * 1024 * 1024;
const BLOCK: u64 = 4 * 1024 * 1024;

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

async fn format_meta(path: &Path) {
    squeezefs::meta_backend::kv::builder::format_v3(
        path,
        VOL_LEN,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .expect("format v3");
}

/// Format + (optionally) stamp bit 15 — the Phase-8 upgrade shape a
/// test volume is born in. Production `format` NEVER stamps it (D9).
async fn format_versioned(path: &Path, stamp: bool) {
    format_meta(path).await;
    if stamp {
        let newly = set_layout_versions_bit(path).await.expect("stamp bit 15");
        assert!(newly, "fresh format must not carry bit 15 (ruling D9)");
    }
}

fn read_superblock(path: &Path) -> SuperblockV3 {
    let mut buf = vec![0u8; SUPERBLOCK_V3_LEN];
    use std::io::Read;
    let mut f = std::fs::File::open(path).unwrap();
    f.read_exact(&mut buf).unwrap();
    SuperblockV3::decode_sector(&buf).expect("live superblock decodes")
}

async fn persisted_layout(routed: &RoutedMetaBackend, ino: u64) -> LayoutMetadata {
    let bytes = routed
        .getxattr(ino, "layout")
        .await
        .expect("getxattr")
        .expect("layout present");
    bincode::deserialize::<LayoutMetadata>(&bytes).expect("folded layout decodes as bincode")
}

fn assert_layout_eq(got: &LayoutMetadata, want: &LayoutMetadata, ctx: &str) {
    assert_eq!(got.file_type, want.file_type, "{ctx}: file_type");
    assert_eq!(got.size, want.size, "{ctx}: size");
    assert_eq!(got.block_map_id, want.block_map_id, "{ctx}: block_map_id");
    assert_eq!(got.file_id, want.file_id, "{ctx}: file_id");
    assert_eq!(got.data_key, want.data_key, "{ctx}: data_key");
    assert_eq!(got.block_map, want.block_map, "{ctx}: block_map");
}

/// One writer-shaped publish round with EXPLICIT §6.2 item-9 versions:
/// grow the expected layout by one block, then commit through the
/// delta-bearing publish call, claiming `base_version` and stamping
/// `version` — exactly what the routing publish pass does with its RAM
/// provenance and the mint.
async fn publish_versioned(
    routed: &RoutedMetaBackend,
    ino: u64,
    layout: &mut LayoutMetadata,
    b: u32,
    base_version: u64,
    version: u64,
) -> squeezefs::error::Result<bool> {
    let key = format!("oss0://{}", b as u64 * BLOCK);
    layout
        .block_map
        .as_mut()
        .expect("map")
        .insert(b, key.clone());
    layout.size = layout.size.max((b as u64 + 1) * BLOCK);
    let full = bincode::serialize(layout).expect("serialize layout");
    let mut delta = LayoutDelta::from_final_state(
        &layout.file_type,
        layout.size,
        layout.block_map_id.as_deref(),
        layout.block_prefix.as_deref(),
        layout.file_id.as_deref(),
        layout.data_key.as_deref(),
        vec![(b, key)],
    );
    delta.set_versions(base_version, version);
    routed
        .merge_layout_and_size(
            ino,
            &delta,
            bytes::Bytes::from(full),
            layout.size,
            Vec::new(),
        )
        .await
}

fn fresh_layout(ino: u64) -> LayoutMetadata {
    LayoutMetadata {
        file_type: "striped".into(),
        size: 0,
        block_map_id: Some(format!("block_map_{ino}")),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(HashMap::new()),
    }
}

// =========================================================================
// Wire contracts (behavioural red: the scaffold encode drops the pair).
// =========================================================================

#[test]
fn versioned_wire_roundtrips_and_strips_byte_identically() {
    let mut d = LayoutDelta::from_final_state(
        "striped",
        8 * BLOCK,
        Some("block_map_9"),
        None,
        None,
        None,
        vec![(7, "oss0://29360128".to_string())],
    );
    let unversioned_bytes = d.encode();

    d.set_versions(0xAA00_0000_0001, 0xAA00_0000_0002);
    let versioned_bytes = d.encode();

    // The pair rides the wire: decode returns it verbatim.
    let back = LayoutDelta::decode(&versioned_bytes).expect("versioned wire decodes");
    assert_eq!(back.base_version, 0xAA00_0000_0001, "base_version rides");
    assert_eq!(back.version, 0xAA00_0000_0002, "version rides");
    assert_eq!(back, d, "whole-struct roundtrip");

    // Exactly 16 bytes of growth — the economy face of the wire.
    assert_eq!(
        versioned_bytes.len(),
        unversioned_bytes.len() + 16,
        "the version pair costs exactly two u64s"
    );

    // The peek (the commit gate's probe) reads the pair without a
    // record decode, and answers None for the unversioned wire.
    assert_eq!(
        layout_delta_versions(&versioned_bytes),
        Some((0xAA00_0000_0001, 0xAA00_0000_0002)),
        "fixed-offset version peek"
    );
    assert_eq!(
        layout_delta_versions(&unversioned_bytes),
        None,
        "unversioned wire has no versions to peek"
    );

    // The strip form (what an UN-stamped volume stores) is
    // byte-identical to the pre-item-9 wire.
    assert_eq!(
        d.encode_unversioned(),
        unversioned_bytes,
        "encode_unversioned must be byte-identical to the zero-version wire \
         (the bit-5 volume compatibility law)"
    );

    // An unversioned record cannot carry a base claim (set_versions
    // zeroes the pair together).
    let mut bare = d.clone();
    bare.set_versions(0x1234, 0);
    assert_eq!(bare.base_version, 0, "version 0 zeroes the base claim");
    assert_eq!(bare.encode(), unversioned_bytes);

    // Strictness: a wire claiming the versioned flag with version 0 is
    // MALFORMED (0 is the reserved unversioned value) — decode refuses.
    let mut forged = versioned_bytes.clone();
    forged[11..19].copy_from_slice(&0u64.to_le_bytes());
    assert!(
        LayoutDelta::decode(&forged).is_err(),
        "versioned flag with version 0 must refuse loud"
    );
}

#[test]
fn bit15_is_disjoint_never_stamped_and_refuses_old_binaries() {
    assert_eq!(FEATURE_INCOMPAT_KV_LAYOUT_VERSIONS.count_ones(), 1);
    assert_eq!(FEATURE_INCOMPAT_KV_LAYOUT_VERSIONS, 1 << 15, "bit 15");
    assert_ne!(
        FEATURES_INCOMPAT_KNOWN & FEATURE_INCOMPAT_KV_LAYOUT_VERSIONS,
        0,
        "this binary must UNDERSTAND bit 15"
    );
    // Ruling D9: every production format (plan) omits it.
    let plan = SuperblockV3::plan(1 << 30, 262_144, None, [7u8; 16], 0x42).expect("plan");
    assert_eq!(
        plan.features_incompat & FEATURE_INCOMPAT_KV_LAYOUT_VERSIONS,
        0,
        "ruling D9: build the bit, never stamp it at format"
    );
    // A pre-item-9 binary (known mask without bit 15) refuses a stamped
    // volume loud — the real gate code path, not a synthetic assert.
    let mut sb = plan;
    sb.features_incompat |= FEATURE_INCOMPAT_KV_LAYOUT_VERSIONS;
    let sector = sb.encode_sector().expect("encode sector");
    assert!(
        SuperblockV3::decode_sector_with_known(
            &sector,
            FEATURES_INCOMPAT_KNOWN & !FEATURE_INCOMPAT_KV_LAYOUT_VERSIONS,
        )
        .is_err(),
        "a pre-item-9 binary must refuse a bit-15 volume loud (KD-14)"
    );
}

// =========================================================================
// Fold contracts (record-level, the §4.2 algebra's new law).
// =========================================================================

fn fold_key() -> Vec<u8> {
    xattr_key(9, 0x00AB_CDEF_9876, 0).to_vec()
}

fn fold_base_layout(n: u32, size: u64) -> LayoutMetadata {
    let mut map = HashMap::new();
    for b in 0..n {
        map.insert(b, format!("oss0://{}", b as u64 * 4096));
    }
    LayoutMetadata {
        file_type: "striped".into(),
        size,
        block_map_id: Some("block_map_9".into()),
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(map),
    }
}

fn fold_base_put(n: u32, size: u64, seq: u64) -> Record {
    let value = bincode::serialize(&fold_base_layout(n, size)).expect("serialize");
    Record::put(
        fold_key(),
        seq,
        XattrValue::encode_parts(b"layout", &value).expect("encode"),
    )
}

fn fold_delta(from: u32, to: u32, size: u64, seq: u64, base_version: u64, version: u64) -> Record {
    let mut d = LayoutDelta {
        file_type: "striped".into(),
        size,
        block_map_id: Some("block_map_9".into()),
        block_prefix: None,
        file_id: None,
        data_key: None,
        entries: (from..to)
            .map(|b| (b, format!("oss0://{}", b as u64 * 4096)))
            .collect(),
        ..Default::default()
    };
    d.set_versions(base_version, version);
    Record {
        key: fold_key(),
        seq,
        kind: RecordKind::Delta,
        value: d.encode(),
    }
}

/// A well-formed versioned chain folds to EXACTLY the layout its
/// unversioned twin folds to (the fold algebra is version-blind on the
/// VALUE — requirement 4: extend, do not disturb), and tie-duplicate
/// records (the node-bset / replay-window overlap the fold input law
/// explicitly allows) stay legal.
#[test]
fn versioned_chain_folds_exactly_like_its_unversioned_twin() {
    let versioned = [
        fold_base_put(2, 2 * 4096, 10),
        fold_delta(2, 5, 5 * 4096, 11, 0, 0x501),
        fold_delta(5, 6, 6 * 4096, 12, 0x501, 0x502),
        fold_delta(6, 9, 9 * 4096, 13, 0x502, 0x503),
    ];
    let unversioned = [
        fold_base_put(2, 2 * 4096, 10),
        fold_delta(2, 5, 5 * 4096, 11, 0, 0),
        fold_delta(5, 6, 6 * 4096, 12, 0, 0),
        fold_delta(6, 9, 9 * 4096, 13, 0, 0),
    ];
    let fv = fold_newest_first(versioned.iter().rev().map(|r| r.record_ref()))
        .expect("a well-formed versioned chain folds");
    let fu = fold_newest_first(unversioned.iter().rev().map(|r| r.record_ref()))
        .expect("unversioned twin folds");
    assert_eq!(
        fv.live_value().expect("live"),
        fu.live_value().expect("live"),
        "versions must not change the folded VALUE"
    );

    // Tie duplicates (same seq presented twice across sources) are
    // identical-effect records and stay legal — the chain-link check
    // must not misread a duplicate as a fork.
    let with_dup = [
        versioned[0].clone(),
        versioned[1].clone(),
        versioned[2].clone(),
        versioned[2].clone(),
        versioned[3].clone(),
    ];
    let fd = fold_newest_first(with_dup.iter().rev().map(|r| r.record_ref()))
        .expect("tie-duplicate records stay legal (identical-effect law)");
    assert_eq!(fd.live_value(), fv.live_value());
}

/// THE item-9 hazard, pinned at the fold: a fork (two links naming the
/// same base) refuses loud instead of folding to a layout NEITHER
/// writer computed.
#[test]
fn divergent_chain_is_refused_loud_at_fold() {
    let recs = [
        fold_base_put(2, 2 * 4096, 10),
        fold_delta(2, 5, 5 * 4096, 11, 0, 0x701),
        // The fork: this link claims the BASE PUT (base 0) while the
        // durable link below it is 0x701 — written by a writer that
        // never saw 0x701.
        fold_delta(5, 6, 6 * 4096, 12, 0, 0x702),
    ];
    let err = fold_newest_first(recs.iter().rev().map(|r| r.record_ref()))
        .expect_err("a forked chain must refuse loud, never fold");
    let msg = format!("{err}");
    assert!(
        msg.contains("divergent") || msg.contains("§6.2"),
        "the refusal must name the divergence, got: {msg}"
    );

    // The wrong-nonzero-claim face of the same fork.
    let recs2 = [
        fold_base_put(2, 2 * 4096, 10),
        fold_delta(2, 5, 5 * 4096, 11, 0, 0x701),
        fold_delta(5, 6, 6 * 4096, 12, 0xDEAD, 0x702),
    ];
    assert!(
        fold_newest_first(recs2.iter().rev().map(|r| r.record_ref())).is_err(),
        "a link naming a base that is not the previous link must refuse"
    );

    // A first link claiming a nonzero base above a bare Put: the commit
    // gate never stages this shape (bare-Put heads admit only claim-0
    // links), so at fold time it is a gate bypass — refuse.
    let recs3 = [
        fold_base_put(2, 2 * 4096, 10),
        fold_delta(2, 5, 5 * 4096, 11, 0xBEEF, 0x703),
    ];
    assert!(
        fold_newest_first(recs3.iter().rev().map(|r| r.record_ref())).is_err(),
        "a first link claiming a nonzero base above a bare Put must refuse"
    );
}

/// Segment homogeneity: one base segment is all-versioned or
/// all-unversioned — the commit gate re-bases before a versioned link
/// can join a pre-stamp chain, so a mix on disk is a gate bypass.
#[test]
fn mixed_version_chains_are_refused_loud_at_fold() {
    // Versioned above unversioned.
    let recs = [
        fold_base_put(1, 4096, 10),
        fold_delta(1, 2, 2 * 4096, 11, 0, 0),
        fold_delta(2, 3, 3 * 4096, 12, 0, 0x801),
    ];
    assert!(
        fold_newest_first(recs.iter().rev().map(|r| r.record_ref())).is_err(),
        "versioned link above an unversioned one must refuse"
    );
    // Unversioned above versioned.
    let recs2 = [
        fold_base_put(1, 4096, 10),
        fold_delta(1, 2, 2 * 4096, 11, 0, 0x801),
        fold_delta(2, 3, 3 * 4096, 12, 0, 0),
    ];
    assert!(
        fold_newest_first(recs2.iter().rev().map(|r| r.record_ref())).is_err(),
        "unversioned link above a versioned one must refuse"
    );
    // And the tombstone algebra is untouched: a Delete shadows a
    // versioned chain exactly as before.
    let recs3 = [
        fold_base_put(1, 4096, 10),
        fold_delta(1, 2, 2 * 4096, 11, 0, 0x801),
        Record::delete(fold_key(), 12),
    ];
    let folded = fold_newest_first(recs3.iter().rev().map(|r| r.record_ref())).expect("fold");
    assert!(matches!(folded, Folded::Tombstone { .. }));
}

// =========================================================================
// Commit-gate contracts (backend integration, stamped volume).
// =========================================================================

/// The acceptance shape (requirement 3): a chain written by a
/// predecessor survives a simulated process restart + term bump and
/// folds IDENTICALLY for the successor — across both the clean-shutdown
/// (bset) face and the drop-without-shutdown (journal replay) face —
/// and the successor's own first publish re-bases (claim 0 against a
/// versioned head ⇒ full Put, `Ok(false)`), then chains versioned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chain_survives_restart_and_term_bump_and_folds_identically() {
    let _s = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "failover", VOL_LEN);
    format_versioned(&meta, true).await;
    let paths = vec![meta.display().to_string()];

    // ---- Predecessor era ----
    let routed = open_routed_meta_set(&paths).await.expect("open");
    let ino = routed
        .create(1, "failover", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create")
        .ino;
    let mut layout = fresh_layout(ino);

    // First-ever persist: full Put (no base to fold onto).
    let used = publish_versioned(&routed, ino, &mut layout, 0, 0, mint_layout_version())
        .await
        .expect("first publish");
    assert!(!used, "first-ever persist has no base to fold onto");

    // Predecessor chains six versioned links.
    let mut prev = 0u64;
    for b in 1..7u32 {
        let v = mint_layout_version();
        assert_ne!(v, 0, "mint must not be exhausted in a test");
        let used = publish_versioned(&routed, ino, &mut layout, b, prev, v)
            .await
            .expect("chained publish");
        assert!(
            used,
            "block {b}: a correctly-claimed link must stage the O(batch) delta \
             (versioning must not regress lever 2)"
        );
        prev = v;
    }
    let want_pre = layout.clone();
    assert_layout_eq(
        &persisted_layout(&routed, ino).await,
        &want_pre,
        "predecessor live fold",
    );

    // ---- Simulated failover: clean shutdown, term bump, reopen ----
    for vol in &routed.volumes {
        vol.shutdown().await.expect("shutdown");
    }
    drop(routed);
    let bumped = adopt_durable_term(durable_term() + 1);
    assert!(bumped > 0, "term bump must adopt");

    let successor = open_routed_meta_set(&paths).await.expect("successor open");
    assert_layout_eq(
        &persisted_layout(&successor, ino).await,
        &want_pre,
        "successor folds the predecessor's chain to the predecessor's exact layout \
         (clean-shutdown face)",
    );

    // The successor's provenance is unknown (claim 0) and the durable
    // head is the predecessor's versioned link: the FIRST publish must
    // RE-BASE with a full Put — never stage an unverifiable link, never
    // error (the first-touch rule).
    let used = publish_versioned(&successor, ino, &mut layout, 7, 0, mint_layout_version())
        .await
        .expect("successor first publish");
    assert!(
        !used,
        "successor's claim-0 publish against a versioned head must re-base with \
         the full Put (Ok(false)), not stage an unverifiable link"
    );
    // After the re-base the head is a bare Put: the successor's next
    // claim-0 link is the segment's first link and stages.
    let v8 = mint_layout_version();
    let used = publish_versioned(&successor, ino, &mut layout, 8, 0, v8)
        .await
        .expect("successor second publish");
    assert!(used, "first link above the successor's re-base must stage");
    // ... and from there the successor chains normally.
    let v9 = mint_layout_version();
    let used = publish_versioned(&successor, ino, &mut layout, 9, v8, v9)
        .await
        .expect("successor third publish");
    assert!(used, "successor chain must continue: claim == head");

    let want_post = layout.clone();
    assert_layout_eq(
        &persisted_layout(&successor, ino).await,
        &want_post,
        "successor live fold",
    );

    // ---- The replay face: drop WITHOUT shutdown ----
    drop(successor);
    let replayed = open_routed_meta_set(&paths).await.expect("replay reopen");
    assert_layout_eq(
        &persisted_layout(&replayed, ino).await,
        &want_post,
        "journal-replay remount folds the versioned chain identically",
    );
    for vol in &replayed.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// THE divergence refusal at commit: a delta claiming a nonzero base
/// that is not the durable chain head is an ERROR naming §6.2 item 9 —
/// never staged, never silently folded, never a silent full-Put
/// clobber — and the durable state is untouched by the refusal. A
/// nonzero claim against a BARE-Put head re-bases instead (compaction
/// legitimately collapses a chain underneath a live writer's RAM
/// provenance, so that shape must never be fatal).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn divergence_is_refused_loud_at_commit_and_state_is_untouched() {
    let _s = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "diverge", VOL_LEN);
    format_versioned(&meta, true).await;
    let routed = open_routed_meta_set(&[meta.display().to_string()])
        .await
        .expect("open");

    // --- Arm A: fork against a versioned head ⇒ ERROR ---
    let ino = routed
        .create(1, "forked", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create")
        .ino;
    let mut layout = fresh_layout(ino);
    publish_versioned(&routed, ino, &mut layout, 0, 0, mint_layout_version())
        .await
        .expect("base publish");
    let v1 = mint_layout_version();
    assert!(
        publish_versioned(&routed, ino, &mut layout, 1, 0, v1)
            .await
            .expect("first link"),
        "first link stages"
    );
    let want = layout.clone();

    // The fork: a writer that believes a base that is NOT the head.
    let mut forked = layout.clone();
    let err = publish_versioned(
        &routed,
        ino,
        &mut forked,
        2,
        0xDEAD_BEEF, // a base nobody durable ever was
        mint_layout_version(),
    )
    .await
    .expect_err("a false base claim must be REFUSED loud (§6.2 item 9)");
    let msg = format!("{err}");
    assert!(
        msg.contains("6.2") || msg.contains("divergen"),
        "the refusal must name the item/divergence, got: {msg}"
    );
    // The refusal staged NOTHING: the durable fold is exactly the
    // pre-fork state.
    assert_layout_eq(
        &persisted_layout(&routed, ino).await,
        &want,
        "refused publish must leave the durable chain untouched",
    );

    // The correctly-claimed link still stages after the refusal.
    let v2 = mint_layout_version();
    assert!(
        publish_versioned(&routed, ino, &mut layout, 2, v1, v2)
            .await
            .expect("correct link after refusal"),
        "a correct claim must stage after a refused fork"
    );

    // --- Arm B: nonzero claim against a BARE-Put head ⇒ re-base, not
    // error (the compaction-collapse tolerance).
    let ino_b = routed
        .create(1, "bare", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create")
        .ino;
    let mut layout_b = fresh_layout(ino_b);
    publish_versioned(&routed, ino_b, &mut layout_b, 0, 0, mint_layout_version())
        .await
        .expect("base publish");
    let used = publish_versioned(
        &routed,
        ino_b,
        &mut layout_b,
        1,
        0xFEED_F00D, // stale RAM provenance after a chain collapse
        mint_layout_version(),
    )
    .await
    .expect("nonzero claim against a bare Put must not error");
    assert!(
        !used,
        "nonzero claim against a bare-Put head re-bases with the full Put"
    );
    assert_layout_eq(
        &persisted_layout(&routed, ino_b).await,
        &layout_b,
        "re-base fold",
    );

    for vol in &routed.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// The old-format rule (forward-only, first touch): a pre-stamp
/// (unversioned) chain on a freshly-stamped volume is folded normally,
/// and the FIRST versioned publish re-bases it with a full Put before
/// any versioned link joins — segments never mix. An unversioned head
/// with a NONZERO claim is a gate bypass and refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_stamp_chain_rebases_on_first_touch_then_chains() {
    let _s = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "upgrade", VOL_LEN);
    // Born UN-stamped: the shipped bit-5 world.
    format_versioned(&meta, false).await;
    let paths = vec![meta.display().to_string()];
    let routed = open_routed_meta_set(&paths).await.expect("open");
    let ino = routed
        .create(1, "legacy", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create")
        .ino;
    let mut layout = fresh_layout(ino);
    // Pre-stamp history: full Put + three unversioned links (versions
    // are stamped on the input struct — an un-stamped volume must strip
    // them, so the durable chain is unversioned regardless).
    publish_versioned(&routed, ino, &mut layout, 0, 0, mint_layout_version())
        .await
        .expect("base");
    for b in 1..4u32 {
        assert!(
            publish_versioned(&routed, ino, &mut layout, b, 0, mint_layout_version())
                .await
                .expect("pre-stamp link"),
            "un-stamped volume keeps staging deltas exactly as shipped"
        );
    }
    for vol in &routed.volumes {
        vol.shutdown().await.expect("shutdown");
    }
    drop(routed);

    // The volume stayed bit-identical on the superblock: no bit 15.
    let sb = read_superblock(&meta);
    assert_eq!(
        sb.features_incompat & FEATURE_INCOMPAT_KV_LAYOUT_VERSIONS,
        0,
        "an un-stamped volume must never grow bit 15 by itself (no ratchet — D9)"
    );

    // Phase-8 upgrade: stamp offline, reopen.
    set_layout_versions_bit(&meta).await.expect("stamp");
    let routed = open_routed_meta_set(&paths).await.expect("reopen stamped");
    assert_layout_eq(
        &persisted_layout(&routed, ino).await,
        &layout,
        "pre-stamp chain folds unchanged after the stamp",
    );

    // A versioned publish with a NONZERO claim against the unversioned
    // head is a bypass shape: refused loud.
    let mut forked = layout.clone();
    assert!(
        publish_versioned(&routed, ino, &mut forked, 4, 0x600D, mint_layout_version())
            .await
            .is_err(),
        "nonzero claim against an unversioned head must refuse loud"
    );

    // First touch (claim 0): re-base with the full Put — the unversioned
    // segment TERMINATES before any versioned link can join it.
    let used = publish_versioned(&routed, ino, &mut layout, 4, 0, mint_layout_version())
        .await
        .expect("first touch");
    assert!(
        !used,
        "first touch of a pre-stamp chain re-bases (full Put)"
    );
    // Then versioned links chain normally.
    let v1 = mint_layout_version();
    assert!(
        publish_versioned(&routed, ino, &mut layout, 5, 0, v1)
            .await
            .expect("first versioned link"),
        "first link above the re-base stages"
    );
    let v2 = mint_layout_version();
    assert!(
        publish_versioned(&routed, ino, &mut layout, 6, v1, v2)
            .await
            .expect("second versioned link"),
        "chained link stages"
    );
    assert_layout_eq(
        &persisted_layout(&routed, ino).await,
        &layout,
        "post-upgrade fold",
    );
    for vol in &routed.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

/// The economy contract (requirement 5, asserted the way S3.5 did — by
/// journal entry counts): the SAME publish sequence on a stamped vs an
/// un-stamped volume pays IDENTICAL journal ENTRIES (the D4 law — one
/// tx, one checksummed entry, versioning adds no commit) and exactly
/// 16 bytes more per DELTA record on the stamped side. The un-stamped
/// side's byte count is the shipped wire verbatim (strip law).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn versioning_adds_no_entries_and_exactly_16_bytes_per_delta() {
    let _s = serial().await;
    let dir = tempfile::tempdir().unwrap();
    const K: u32 = 24; // < the 64 chain cap: no re-bases in the window

    let mut observed: Vec<(u64, u64)> = Vec::new(); // (entries, bytes) per volume
    for (name, stamp) in [("stamped", true), ("plain", false)] {
        let meta = make_file(dir.path(), name, VOL_LEN);
        format_versioned(&meta, stamp).await;
        let routed = open_routed_meta_set(&[meta.display().to_string()])
            .await
            .expect("open");
        let ino = routed
            .create(1, "economy", libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("create")
            .ino;
        let mut layout = fresh_layout(ino);
        let delta_before = META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed);
        let full_before = META_KV_LAYOUT_FULL_COMMITS.load(Ordering::Relaxed);

        // Identical sequence on both volumes, versions stamped on the
        // input struct on BOTH (the routing layer always mints; the
        // backend strips on the un-stamped side).
        publish_versioned(&routed, ino, &mut layout, 0, 0, 0x9000)
            .await
            .expect("base");
        let ring = routed.volumes[0].journal_ring();
        let e0 = ring.written_entries();
        let b0 = ring.written_bytes();
        let mut prev = 0u64;
        for b in 1..K {
            let v = 0x9000 + b as u64;
            assert!(
                publish_versioned(&routed, ino, &mut layout, b, prev, v)
                    .await
                    .expect("link"),
                "{name}: link {b} stages"
            );
            prev = v;
        }
        let entries = ring.written_entries() - e0;
        let bytes = ring.written_bytes() - b0;
        assert_eq!(
            META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed) - delta_before,
            (K - 1) as u64,
            "{name}: every post-base publish stages a delta"
        );
        assert_eq!(
            META_KV_LAYOUT_FULL_COMMITS.load(Ordering::Relaxed) - full_before,
            1,
            "{name}: exactly the first publish goes full"
        );
        assert_layout_eq(&persisted_layout(&routed, ino).await, &layout, name);
        for vol in &routed.volumes {
            vol.shutdown().await.expect("shutdown");
        }
        observed.push((entries, bytes));
    }

    let (entries_stamped, bytes_stamped) = observed[0];
    let (entries_plain, bytes_plain) = observed[1];
    assert_eq!(
        entries_stamped, entries_plain,
        "the D4 law: versioning must add ZERO journal entries"
    );
    assert_eq!(
        bytes_stamped - bytes_plain,
        16 * (K as u64 - 1),
        "the version pair costs exactly 16 bytes per delta record and NOTHING \
         on the un-stamped volume (strip law)"
    );
}

/// Concurrency (requirement: `multi_thread`): distinct inos chain
/// versioned links concurrently through the Lever B aggregated
/// conveyor on ONE stamped volume — every link stages, every fold is
/// exact, no false divergence refusals.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_inos_chain_versioned_links_without_false_refusals() {
    let _s = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "concurrent", VOL_LEN);
    format_versioned(&meta, true).await;
    let routed = Arc::new(
        open_routed_meta_set(&[meta.display().to_string()])
            .await
            .expect("open"),
    );

    const TASKS: usize = 6;
    const LINKS: u32 = 20;
    let mut handles = Vec::new();
    for t in 0..TASKS {
        let routed = Arc::clone(&routed);
        handles.push(tokio::spawn(async move {
            let ino = routed
                .create(1, &format!("c{t}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .expect("create")
                .ino;
            let mut layout = fresh_layout(ino);
            publish_versioned(&routed, ino, &mut layout, 0, 0, mint_layout_version())
                .await
                .expect("base");
            let mut prev = 0u64;
            for b in 1..LINKS {
                let v = mint_layout_version();
                let used = publish_versioned(&routed, ino, &mut layout, b, prev, v)
                    .await
                    .expect("no false refusals under concurrency");
                assert!(used, "task {t} link {b} must stage");
                prev = v;
            }
            (ino, layout)
        }));
    }
    for h in handles {
        let (ino, want) = h.await.expect("task");
        assert_layout_eq(
            &persisted_layout(&routed, ino).await,
            &want,
            "concurrent fold",
        );
    }
    for vol in &routed.volumes {
        vol.shutdown().await.expect("shutdown");
    }
}

// =========================================================================
// Lever A stays engaged (requirement 2) — the fs-level publish path on a
// versions-stamped volume: base-provenance ledger closes against the
// pass count, the delta path stays engaged, bytes read back exact.
// =========================================================================

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _s: TempDir,
}

const FS_BS: u64 = 65536;

async fn make_fs(meta: &Path, stamp: bool) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", FS_BS.to_string());
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new("mwlv0").await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    squeezefs::meta_backend::kv::builder::format_v3(
        meta,
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
    .unwrap();
    if stamp {
        set_layout_versions_bit(meta).await.expect("stamp bit 15");
    }
    let be = KvMetaBackend::open(meta).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs,
        req,
        _b: b,
        _s: s,
    }
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

/// Lever A closure on a versions-stamped volume: every publish pass's
/// RMW base is accounted (dirty + ram + fetches == passes), the delta
/// path engages (versioned chains — `layout_delta_commits` grows), and
/// the read-back is exact. This is the requirement-2 non-regression
/// pin: the 7.7× publish-tax fix must keep serving while the base name
/// is durable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lever_a_ledger_closes_on_a_versions_stamped_volume() {
    let _s = serial().await;
    // The rewrite-shadow epoch records rewrite publishes RAM-only; this
    // contract pins the DURABLE publish machinery, so the lever is off
    // (the publish_drain_economy_tests discipline).
    struct ShadowOff;
    impl Drop for ShadowOff {
        fn drop(&mut self) {
            squeezefs::routing::set_rewrite_shadow(true);
        }
    }
    squeezefs::routing::set_rewrite_shadow(false);
    let _shadow = ShadowOff;

    let meta_file = NamedTempFile::new().unwrap();
    let h = make_fs(meta_file.path(), true).await;
    let ino =
        h.fs.create(h.req, 1, OsStr::new("levera"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;

    let d0 = METRICS.publish_base_dirty_serves.load(Ordering::Relaxed)
        + METRICS.publish_base_ram_serves.load(Ordering::Relaxed)
        + METRICS.publish_base_fetches.load(Ordering::Relaxed);
    let p0 = METRICS.layout_publish_batches.load(Ordering::Relaxed);
    let dc0 = META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed);

    const BLOCKS: u32 = 8;
    for pass in 0..2u8 {
        for b in 0..BLOCKS {
            let data = pattern(FS_BS as usize, 0x30 ^ pass ^ (b as u8));
            let w =
                h.fs.write(
                    h.req,
                    ino,
                    0,
                    b as u64 * FS_BS,
                    bytes::Bytes::copy_from_slice(&data),
                    0,
                    0,
                )
                .await
                .unwrap();
            assert_eq!(w.written as usize, data.len());
        }
        h.fs.fsync(h.req, ino, 0, false).await.unwrap();
        assert!(
            h.fs.write_pipeline
                .quiesce(std::time::Duration::from_secs(30))
                .await,
            "pipeline must drain"
        );
    }

    let served = METRICS.publish_base_dirty_serves.load(Ordering::Relaxed)
        + METRICS.publish_base_ram_serves.load(Ordering::Relaxed)
        + METRICS.publish_base_fetches.load(Ordering::Relaxed)
        - d0;
    let passes = METRICS.layout_publish_batches.load(Ordering::Relaxed) - p0;
    assert!(passes >= 1, "the publish conveyor must have run");
    assert_eq!(
        served, passes,
        "the base-provenance ledger must close EXACTLY against the pass count \
         (Lever A regression tripwire)"
    );
    assert!(
        META_KV_LAYOUT_DELTA_COMMITS.load(Ordering::Relaxed) > dc0,
        "the delta path must stay engaged on a versions-stamped volume \
         (lever 2 regression tripwire)"
    );

    // Correctness face: the last pass's bytes read back exact.
    for b in 0..BLOCKS {
        let want = pattern(FS_BS as usize, 0x30 ^ 1 ^ (b as u8));
        let got =
            h.fs.read(h.req, ino, 0, b as u64 * FS_BS, FS_BS as u32, 0)
                .await
                .unwrap()
                .data
                .to_vec();
        assert_eq!(got, want, "block {b} read-back after versioned publishes");
    }
}
