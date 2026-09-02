//! Writer-scoped staging — pre-RC engineering spec §6.2 **items 8 and
//! 10** under ruling **D9** (incompat bit 10 is BUILT, never STAMPED).
//!
//! The two single-writer assumptions this suite pins:
//!
//! * **item 8** — `active_block:` / `active_block_ext:` / `mapping:` keys
//!   had no writer scope, so they were SHARED keys naming node-PRIVATE
//!   staging payloads: recovery could not classify a foreign record, and a
//!   second writer's staged work was indistinguishable from ours;
//! * **item 10** — the staging generation stamp was the volume-set uuids
//!   only, i.e. identical on every node: node B's staged payloads passed
//!   node A's generation gate.
//!
//! Contracts, by direction:
//!
//! 1. **Compatibility (D9's hard requirement)** — an un-stamped volume
//!    set mints byte-identical keys, stamps byte-identical marker bytes,
//!    keeps staging content-format v2, and recovers staged work written by
//!    a pre-change binary.
//! 2. **Key scoping** — the scope is the LAST key component, so every
//!    historical scan prefix still matches (consistent with the durable
//!    block-refcount key layout's reserved writer-id position); stack and
//!    heap mints agree; parsers tolerate the component; classification is
//!    Legacy (grandfathered ours) / Mine / Foreign / Unprovable.
//! 3. **Node identity** — host-stable, reboot-stable, process-independent,
//!    distinct across hosts; unusable material refuses rather than
//!    synthesizing a confident-looking token.
//! 4. **Recovery classification** — a foreign-scoped record is never
//!    adopted, never budget-counted, never folded, never flushed, never
//!    wiped: it is counted and left intact. Ours (and legacy) keep every
//!    landed law (generation-bound, fencing-stamped, torn-discard,
//!    future-refuse).
//! 5. **The root-level gate** — same-node warm restart keeps content; a
//!    bare marker on a scoped set is the Phase-8 UPGRADE (adopt +
//!    re-stamp, never discard); a foreign node's root holding LIVE custody
//!    REFUSES the mount loud (never wipes a peer's acked custody); with no
//!    live custody it is discarded losslessly; a foreign SET still hits
//!    the landed dead-generation discard.
//! 6. **KD-8 composition** — the crash-safe two-phase staged-payload
//!    rebind still adopts under node scoping, including the strand class
//!    that would otherwise DISCARD durable acked staged data (the
//!    data-loss pin).
//! 7. **Bit hygiene** — every incompat constant is a distinct single bit,
//!    the known mask covers them, and `plan()` never stamps bit 10.

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::nvme::{
    staging_format_write_version, STAGING_FORMAT_MARKER, STAGING_FORMAT_VERSION,
    STAGING_GENERATION_MARKER,
};
use squeezefs::cache::TieredCache;
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::writer_scope as ws;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::{tempdir, NamedTempFile};

const BS: u64 = 65536;

static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

/// The writer scope is process-global (mount-time, once), so every test
/// that engages it runs serially and restores the disengaged default.
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

struct ScopeGuard;

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        ws::engage(None);
    }
}

/// Engage the NODE-ONLY scope (the pre-pair form every existing contract
/// pins; offline-process identity).
fn engage(token: u64) -> ScopeGuard {
    ws::engage(Some(ws::WriterScope::node_only(token)));
    ScopeGuard
}

/// Engage a PAIR scope (KD-MW-2: a mount's `(node, mount_slot)` identity).
fn engage_pair(token: u64, slot: u32) -> ScopeGuard {
    ws::engage(Some(ws::WriterScope::new(token, slot)));
    ScopeGuard
}

/// Shorthand for the node-only scope value.
fn node(token: u64) -> ws::WriterScope {
    ws::WriterScope::node_only(token)
}

/// Shorthand for a pair scope value.
fn pair(token: u64, slot: u32) -> ws::WriterScope {
    ws::WriterScope::new(token, slot)
}

const NODE_A: u64 = 0x0123_4567_89ab_cdef;
const NODE_B: u64 = 0xfedc_ba98_7654_3210;

fn suffix(token: u64) -> String {
    format!(":w_{token:016x}")
}

/// A bare staging cache bound to `staging_generation` — the residue
/// factory (dropping it is the kill-9 equivalent for RAM state).
async fn cache_at(
    staging: &Path,
    data: &Path,
    alloc: &str,
    staging_generation: Option<&str>,
) -> squeezefs::error::Result<TieredCache> {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    let nvme = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new(alloc).await.unwrap());
    TieredCache::new(
        vec![staging.to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("8MB"),
        ba,
        nvme,
        staging_generation,
    )
    .await
}

fn marker(staging: &Path) -> Option<String> {
    std::fs::read_to_string(staging.join(STAGING_GENERATION_MARKER)).ok()
}

fn format_marker(staging: &Path) -> Option<String> {
    std::fs::read_to_string(staging.join(STAGING_FORMAT_MARKER)).ok()
}

// ---------------------------------------------------------------------------
// 1. Compatibility — the un-stamped volume behaves EXACTLY as today
// ---------------------------------------------------------------------------

/// Contract 1a: with no scope engaged (every shipped volume — D9: nothing
/// stamps bit 10) every key helper emits its historical spelling, byte for
/// byte. This is the assertion that makes the whole change invisible to
/// existing volumes.
#[tokio::test]
async fn disengaged_keys_are_byte_identical_to_prior_releases() {
    let _g = serial().await;
    ws::engage(None);
    assert_eq!(
        &*squeezefs::keys::active_block(7, 3),
        "active_block:inode_7:block_3"
    );
    assert_eq!(
        &*squeezefs::keys::active_block_ext(7, 3),
        "active_block_ext:inode_7:block_3"
    );
    assert_eq!(
        &*squeezefs::keys::active_block_for_path("inode_7", 3),
        "active_block:inode_7:block_3"
    );
    assert_eq!(
        &*squeezefs::keys::active_block_ext_for_path("inode_7", 3),
        "active_block_ext:inode_7:block_3"
    );
    assert_eq!(&*squeezefs::keys::mapping("fid"), "mapping:fid");
    assert_eq!(
        &*squeezefs::keys::active_block_stack(7, 3),
        "active_block:inode_7:block_3"
    );
    assert_eq!(
        &*squeezefs::keys::active_block_path_stack("inode_7", 3).unwrap(),
        "active_block:inode_7:block_3"
    );
    assert_eq!(
        &*squeezefs::keys::active_block_ext_path_stack("inode_7", 3).unwrap(),
        "active_block_ext:inode_7:block_3"
    );
    assert_eq!(
        ws::staging_generation("v3:aabb", None),
        "v3:aabb",
        "an un-scoped staging generation is the set generation VERBATIM"
    );
    assert_eq!(
        staging_format_write_version(false),
        2,
        "an un-scoped mount keeps stamping staging content-format v2, so an \
         older binary can still adopt the root"
    );
}

/// Contract 1b: an un-stamped mount stamps the historical marker bytes and
/// the v2 content-format marker, and a warm restart keeps its content —
/// the pre-change binary's staged work recovers unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unstamped_set_marker_bytes_and_recovery_are_unchanged() {
    let _g = serial().await;
    ws::engage(None);
    let staging = tempdir().unwrap();
    let data = NamedTempFile::new().unwrap();
    let gen = "v3:00112233445566778899aabbccddeeff";

    // Session A: a "pre-change binary" (no scope anywhere) stages custody.
    {
        let c = cache_at(staging.path(), data.path(), "compat-a", Some(gen))
            .await
            .unwrap();
        assert!(c.nvme.put_active_block(
            "active_block:inode_9:block_0",
            &vec![0x5Au8; BS as usize],
            7
        ));
        c.nvme
            .stage_write(
                "inode_9",
                "compat-file-id",
                bytes::Bytes::from(vec![1u8; 8192]),
                7,
            )
            .await
            .unwrap();
    }
    assert_eq!(
        marker(staging.path()).unwrap(),
        format!("squeezefs-staging-generation-v1\n{gen}\n"),
        "the marker image must be byte-identical to prior releases"
    );
    assert_eq!(
        format_marker(staging.path()).unwrap(),
        "squeezefs-staging-format-v1\n2\n",
        "un-scoped mounts stamp content-format v2"
    );

    // Session B: the same un-stamped set — warm restart keeps everything.
    let c = cache_at(staging.path(), data.path(), "compat-b", Some(gen))
        .await
        .unwrap();
    let keys = c.nvme.list_staged_files();
    assert!(
        keys.iter().any(|k| k == "active_block:inode_9:block_0"),
        "pre-change active-block custody must recover: {keys:?}"
    );
    assert!(
        c.nvme
            .has_staged_active_block("active_block:inode_9:block_0"),
        "recovered custody must be occupancy-indexed"
    );
    assert!(
        c.nvme.current_staged_write_bytes() > 0,
        "the staged file must seed the budget ledger"
    );
}

/// Contract 1c: a SCOPED mount over a staging root written by a
/// pre-change binary adopts it — the records are unscoped (legacy) and
/// grandfathered as ours, and the marker upgrades in place instead of the
/// content being discarded. This is the Phase-8 upgrade path, and the
/// reason it must never discard: those bytes are acked staged custody.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_upgrade_adopts_pre_change_staging_without_discarding() {
    let _g = serial().await;
    let staging = tempdir().unwrap();
    let data = NamedTempFile::new().unwrap();
    let set = "v3:0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";

    ws::engage(None);
    {
        let c = cache_at(staging.path(), data.path(), "upg-a", Some(set))
            .await
            .unwrap();
        assert!(c.nvme.put_active_block(
            "active_block:inode_4:block_0",
            &vec![0x11u8; BS as usize],
            7
        ));
    }

    let discards_before = staging_discards();
    let upgrades_before = scope_upgrades();
    let _sg = engage(NODE_A);
    let scoped = ws::staging_generation(set, Some(node(NODE_A)));
    let c = cache_at(staging.path(), data.path(), "upg-b", Some(&scoped))
        .await
        .unwrap();
    assert_eq!(
        staging_discards(),
        discards_before,
        "the upgrade must NOT take the dead-generation discard arm"
    );
    assert_eq!(
        scope_upgrades(),
        upgrades_before + 1,
        "the upgrade arm must be counted (staging_scope_upgrades)"
    );
    assert_eq!(
        marker(staging.path()).unwrap(),
        format!("squeezefs-staging-generation-v1\n{scoped}\n"),
        "the root must be re-stamped node-scoped"
    );
    assert_eq!(
        format_marker(staging.path()).unwrap(),
        "squeezefs-staging-format-v1\n3\n",
        "a scoped mount stamps content-format v3 (the downgrade fence)"
    );
    assert!(
        c.nvme
            .list_staged_files()
            .iter()
            .any(|k| k == "active_block:inode_4:block_0"),
        "legacy (unscoped) custody must be adopted by the scoped mount"
    );
    assert!(
        c.nvme
            .has_staged_active_block("active_block:inode_4:block_0"),
        "legacy custody is ours by grandfathering — it must be indexed"
    );
}

fn staging_discards() -> u64 {
    squeezefs::fuse_client::METRICS
        .staging_generation_discards
        .load(Ordering::Relaxed)
}

fn scope_upgrades() -> u64 {
    squeezefs::fuse_client::METRICS
        .staging_scope_upgrades
        .load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// 2. Key scoping (item 8)
// ---------------------------------------------------------------------------

/// The scope is the LAST component, after the identity components — the
/// same reservation `TREE_BLOCK_REFS` made for a writer id after
/// `block_index` (docs/design-durable-block-refcounts.md §3.2). The
/// consequence pinned here: every historical scan prefix still matches its
/// own records, which is what lets recovery SEE a record in order to
/// classify it.
#[tokio::test]
async fn engaged_keys_append_the_scope_after_the_identity_components() {
    let _g = serial().await;
    let _sg = engage(NODE_A);
    let s = suffix(NODE_A);

    assert_eq!(
        &*squeezefs::keys::active_block(7, 3),
        format!("active_block:inode_7:block_3{s}")
    );
    assert_eq!(
        &*squeezefs::keys::active_block_ext(7, 3),
        format!("active_block_ext:inode_7:block_3{s}")
    );
    assert_eq!(&*squeezefs::keys::mapping("fid"), format!("mapping:fid{s}"));

    // Prefix compatibility: the identity prefixes are deliberately
    // UNSCOPED and must still match.
    let key = squeezefs::keys::active_block(7, 3).to_string();
    assert!(key.starts_with(squeezefs::keys::active_block_ino_prefix(7).as_str()));
    assert!(key.starts_with(squeezefs::keys::active_block_path_prefix("inode_7").as_str()));
    let ext = squeezefs::keys::active_block_ext(7, 3).to_string();
    assert!(ext.starts_with(squeezefs::keys::active_block_ext_ino_prefix(7).as_str()));
    assert!(
        squeezefs::cache::nvme::key_is_block_family(&key),
        "family classification is prefix-based and unaffected"
    );
}

/// Stack (zero-heap) and heap mints must agree in BOTH arms — a divergence
/// would make the warm fast-path probe miss our own staged bytes.
#[tokio::test]
async fn stack_and_heap_mints_agree_in_both_arms() {
    let _g = serial().await;
    {
        ws::engage(None);
        assert_eq!(
            &*squeezefs::keys::active_block_stack(12, 5),
            &*squeezefs::keys::active_block(12, 5)
        );
        assert_eq!(
            &*squeezefs::keys::active_block_path_stack("inode_12", 5).unwrap(),
            &*squeezefs::keys::active_block_for_path("inode_12", 5)
        );
        assert_eq!(
            &*squeezefs::keys::active_block_ext_path_stack("inode_12", 5).unwrap(),
            &*squeezefs::keys::active_block_ext_for_path("inode_12", 5)
        );
    }
    let _sg = engage(NODE_B);
    assert_eq!(
        &*squeezefs::keys::active_block_stack(12, 5),
        &*squeezefs::keys::active_block(12, 5)
    );
    assert_eq!(
        &*squeezefs::keys::active_block_path_stack("inode_12", 5).unwrap(),
        &*squeezefs::keys::active_block_for_path("inode_12", 5)
    );
    assert_eq!(
        &*squeezefs::keys::active_block_ext_path_stack("inode_12", 5).unwrap(),
        &*squeezefs::keys::active_block_ext_for_path("inode_12", 5)
    );
    // A path so long that the stack form cannot hold key + scope must
    // FAIL, not truncate and not silently drop the scope.
    let long = "x".repeat(180);
    assert!(
        squeezefs::keys::active_block_path_stack(&long, 1).is_none(),
        "overflow must return None so the caller takes the heap (scoped) form"
    );
}

/// Classification: Legacy (grandfathered ours), Mine, Foreign, and the
/// Unprovable downgrade direction. Plus the hostile-tail cases that must
/// NOT parse as a scope.
#[tokio::test]
async fn key_classification_covers_legacy_mine_foreign_and_unprovable() {
    let _g = serial().await;
    let a = format!("active_block:inode_1:block_0{}", suffix(NODE_A));
    let b = format!("active_block:inode_1:block_0{}", suffix(NODE_B));
    let legacy = "active_block:inode_1:block_0".to_string();

    {
        let _sg = engage(NODE_A);
        assert_eq!(ws::classify_key(&a), ws::KeyOwner::Mine);
        assert_eq!(ws::classify_key(&b), ws::KeyOwner::Foreign(node(NODE_B)));
        assert_eq!(ws::classify_key(&legacy), ws::KeyOwner::Legacy);
        assert!(ws::key_is_mine(&a) && ws::key_is_mine(&legacy));
        assert!(!ws::key_is_mine(&b));
    }
    ws::engage(None);
    assert_eq!(
        ws::classify_key(&a),
        ws::KeyOwner::Unprovable(node(NODE_A)),
        "a scoped record under an unscoped mount is unprovable, never ours"
    );
    assert!(!ws::key_is_mine(&a));
    assert_eq!(ws::classify_key(&legacy), ws::KeyOwner::Legacy);

    // Scope stripping is exact and idempotent.
    assert_eq!(ws::strip_key_scope(&a), legacy);
    assert_eq!(ws::strip_key_scope(&legacy), legacy);
    // Hostile tails: wrong length, uppercase (we mint lowercase — two
    // spellings of one scope would be a classification hole), non-hex.
    for bad in [
        "active_block:inode_1:block_0:w_0123",
        "active_block:inode_1:block_0:w_0123456789ABCDEF",
        "active_block:inode_1:block_0:w_0123456789abcdeg",
        "active_block:inode_1:block_0:x_0123456789abcdef",
        ":w_0123456789abcdef",
    ] {
        if bad == ":w_0123456789abcdef" {
            // Degenerate but well-formed: the parser reports the scope; no
            // real key has this shape (every family carries a prefix).
            assert!(ws::key_scope(bad).is_some());
            continue;
        }
        assert!(
            ws::key_scope(bad).is_none(),
            "must not parse a scope out of {bad}"
        );
    }
    // No unscoped key of any shipped release ends in a parseable scope.
    for shipped in [
        "active_block:inode_1:block_0",
        "active_block_ext:inode_1:block_0",
        "mapping:6f8f57715090da2632453988d9a1501b",
        "inode_42",
    ] {
        assert!(ws::key_scope(shipped).is_none(), "{shipped}");
    }
}

// ---------------------------------------------------------------------------
// 3. Node identity (item 10)
// ---------------------------------------------------------------------------

/// The identity requirements, mechanized:
///
/// * reboot-stable and process-independent — the SAME source bytes always
///   derive the SAME token, which is why the source is a file in `/etc`
///   (never a boot id, never a per-mount uuid). Process independence is
///   what lets the OFFLINE KD-8 rebind agree with the daemon;
/// * host-distinct — different bytes derive different tokens;
/// * refusal over invention — material that cannot identify a host is
///   rejected instead of hashed into a confident-looking token.
#[tokio::test]
async fn node_identity_is_stable_process_independent_and_host_distinct() {
    let _g = serial().await;
    let dir = tempdir().unwrap();
    let id_a = dir.path().join("id-a");
    let id_b = dir.path().join("id-b");
    std::fs::write(&id_a, "6b9c1f0e4d3a4b8c9e1f2a3b4c5d6e7f\n").unwrap();
    std::fs::write(&id_b, "aaaabbbbccccddddeeeeffff00001111\n").unwrap();

    let saved = std::env::var("SQUEEZEFS_NODE_ID_FILE").ok();
    std::env::set_var("SQUEEZEFS_NODE_ID_FILE", &id_a);
    let first = ws::resolve_node_identity().expect("id-a resolves");
    let again = ws::resolve_node_identity().expect("id-a resolves again");
    assert_eq!(
        first.token, again.token,
        "a stable source must derive a stable token (reboot survival)"
    );
    assert_eq!(first.source, ws::NodeIdSource::EnvFile);
    std::env::set_var("SQUEEZEFS_NODE_ID_FILE", &id_b);
    let other = ws::resolve_node_identity().expect("id-b resolves");
    assert_ne!(
        first.token, other.token,
        "distinct hosts must derive distinct tokens"
    );

    // Unusable material falls THROUGH to the next source (it never
    // becomes a token): with the env source unusable, resolution lands on
    // a system source or refuses loud — either way not on `id-c`.
    let id_c = dir.path().join("id-c");
    std::fs::write(&id_c, "00000000000000000000000000000000\n").unwrap();
    std::env::set_var("SQUEEZEFS_NODE_ID_FILE", &id_c);
    match ws::resolve_node_identity() {
        Ok(id) => assert_ne!(
            id.source,
            ws::NodeIdSource::EnvFile,
            "the all-zero uninitialized machine-id must never be adopted"
        ),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("/etc/machine-id") && msg.contains("node-id"),
                "the refusal must name every source it tried and the remedy: {msg}"
            );
        }
    }
    match saved {
        Some(v) => std::env::set_var("SQUEEZEFS_NODE_ID_FILE", v),
        None => std::env::remove_var("SQUEEZEFS_NODE_ID_FILE"),
    }
}

/// The staging generation's decoration round-trips, and the split tolerates
/// every pre-change (un-decorated) marker value.
#[tokio::test]
async fn staging_generation_decoration_round_trips() {
    let set = "v3:aabbccddeeff00112233445566778899|v3:00112233445566778899aabbccddeeff";
    let scoped = ws::staging_generation(set, Some(node(NODE_A)));
    assert_eq!(scoped, format!("{set}@node:{NODE_A:016x}"));
    assert_eq!(
        ws::split_staging_generation(&scoped),
        (set, Some(node(NODE_A)))
    );
    assert_eq!(ws::split_staging_generation(set), (set, None));
    // Garbled decorations degrade to "the whole thing is the set part",
    // never to a wrong node token.
    for bad in [
        format!("{set}@node:short"),
        format!("{set}@node:GGGGGGGGGGGGGGGG"),
        format!("{set}@node:"),
    ] {
        assert_eq!(ws::split_staging_generation(&bad).1, None, "{bad}");
    }
}

/// The classification table the root-level gate and the KD-8 barrier share
/// (one function, so they cannot drift).
#[tokio::test]
async fn generation_classification_table() {
    let set = "v3:1111111111111111111111111111111f";
    let other = "v3:2222222222222222222222222222222f";
    let a = ws::staging_generation(set, Some(node(NODE_A)));
    let b = ws::staging_generation(set, Some(node(NODE_B)));

    assert_eq!(
        ws::classify_generation(&a, &a, Some(node(NODE_A))),
        ws::GenerationBinding::Match
    );
    assert_eq!(
        ws::classify_generation(set, set, None),
        ws::GenerationBinding::Match
    );
    assert_eq!(
        ws::classify_generation(set, &a, Some(node(NODE_A))),
        ws::GenerationBinding::ScopeUpgrade,
        "a bare marker on a scoped set is the upgrade arm, not a discard"
    );
    assert_eq!(
        ws::classify_generation(&b, &a, Some(node(NODE_A))),
        ws::GenerationBinding::ForeignScope(node(NODE_B))
    );
    assert_eq!(
        ws::classify_generation(&a, set, None),
        ws::GenerationBinding::ForeignScope(node(NODE_A)),
        "an unscoped mount cannot claim a scoped root"
    );
    assert_eq!(
        ws::classify_generation(other, &a, Some(node(NODE_A))),
        ws::GenerationBinding::ForeignSet
    );
    assert_eq!(
        ws::classify_generation(
            &ws::staging_generation(other, Some(node(NODE_A))),
            &a,
            Some(node(NODE_A))
        ),
        ws::GenerationBinding::ForeignSet,
        "the SET part decides first — our own scope on a dead set is dead"
    );
}

// ---------------------------------------------------------------------------
// 4/5. The root-level gate and record-level recovery classification
// ---------------------------------------------------------------------------

/// A peer node's staging root that holds LIVE staged write custody
/// REFUSES the mount, loudly, and leaves every byte in place. Wiping it
/// would destroy another writer's acked staged payloads, and we cannot
/// flush them either — their payload ring is that node's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_node_root_with_live_custody_refuses_the_mount() {
    let _g = serial().await;
    let staging = tempdir().unwrap();
    let data = NamedTempFile::new().unwrap();
    let set = "v3:3333333333333333333333333333333f";

    // Node B populates the root (live custody, no clean drain).
    {
        let _sg = engage(NODE_B);
        let gen_b = ws::staging_generation(set, Some(node(NODE_B)));
        let c = cache_at(staging.path(), data.path(), "foreign-b", Some(&gen_b))
            .await
            .unwrap();
        assert!(c.nvme.put_active_block(
            &squeezefs::keys::active_block(3, 0),
            &vec![0x77u8; BS as usize],
            9
        ));
    }
    let before = squeezefs::fuse_client::METRICS
        .staging_foreign_scope_refusals
        .load(Ordering::Relaxed);

    // Node A mounts the same root: refusal, not a wipe.
    let _sg = engage(NODE_A);
    let gen_a = ws::staging_generation(set, Some(node(NODE_A)));
    let msg = match cache_at(staging.path(), data.path(), "foreign-a", Some(&gen_a)).await {
        Ok(_) => panic!("a peer's live custody must refuse the staging root as a unit"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("WRITER-SCOPE REFUSAL") && msg.contains(&format!("w_{NODE_B:016x}")),
        "the refusal must name the class and the foreign scope: {msg}"
    );
    assert!(
        msg.contains("set-cache-paths") || msg.contains("node-private"),
        "the refusal must name the remedy: {msg}"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .staging_foreign_scope_refusals
            .load(Ordering::Relaxed),
        before + 1,
        "the refusal is a counted tripwire"
    );
    assert_eq!(
        marker(staging.path()).unwrap(),
        format!(
            "squeezefs-staging-generation-v1\n{}\n",
            ws::staging_generation(set, Some(node(NODE_B)))
        ),
        "the peer's marker must be untouched"
    );
    assert!(
        squeezefs::cache::nvme::dir_has_segment_data(&staging.path().join("staging_segment")),
        "the peer's staged bytes must still be on disk"
    );
}

/// A foreign-scoped root with NO live custody is dead content: discarding
/// it is lossless, so the mount proceeds (counted separately from the
/// dead-generation class so the security/ownership classes stay clean).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_node_root_without_live_custody_is_discarded_losslessly() {
    let _g = serial().await;
    let staging = tempdir().unwrap();
    let data = NamedTempFile::new().unwrap();
    let set = "v3:4444444444444444444444444444444f";

    // Node B's root: a marker, a read-cache segment tree, no live custody.
    std::fs::create_dir_all(staging.path().join("staging_segment")).unwrap();
    squeezefs::cache::write_staging_generation_marker(
        staging.path(),
        &ws::staging_generation(set, Some(node(NODE_B))),
    )
    .await
    .unwrap();

    let before = squeezefs::fuse_client::METRICS
        .staging_foreign_scope_discards
        .load(Ordering::Relaxed);
    let _sg = engage(NODE_A);
    let gen_a = ws::staging_generation(set, Some(node(NODE_A)));
    let _c = cache_at(staging.path(), data.path(), "foreign-dead", Some(&gen_a))
        .await
        .expect("a dead foreign root must not block the mount");
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .staging_foreign_scope_discards
            .load(Ordering::Relaxed),
        before + 1
    );
    assert_eq!(
        marker(staging.path()).unwrap(),
        format!("squeezefs-staging-generation-v1\n{gen_a}\n"),
        "the root must be re-stamped to ours"
    );
}

/// A record scoped to another writer that nonetheless appears in our ring
/// is never adopted: not occupancy-indexed (so no probe serves it), not
/// budget-counted, and left intact — with the tripwire counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_scoped_records_are_never_adopted_and_never_wiped() {
    let _g = serial().await;
    let staging = tempdir().unwrap();
    let data = NamedTempFile::new().unwrap();
    let set = "v3:5555555555555555555555555555555f";
    let gen_a = ws::staging_generation(set, Some(node(NODE_A)));
    let foreign_key = format!("active_block:inode_8:block_0{}", suffix(NODE_B));
    let mine_key = format!("active_block:inode_8:block_1{}", suffix(NODE_A));

    let _sg = engage(NODE_A);
    // Session 1 seeds BOTH: ours and a foreign-scoped record in the same
    // ring (the shared-root hazard, forced).
    {
        let c = cache_at(staging.path(), data.path(), "mixed-1", Some(&gen_a))
            .await
            .unwrap();
        assert!(c
            .nvme
            .put_active_block(&mine_key, &vec![0x21u8; BS as usize], 7));
        assert!(c
            .nvme
            .put_active_block(&foreign_key, &vec![0x22u8; BS as usize], 7));
    }

    let before = squeezefs::fuse_client::METRICS
        .staging_foreign_scope_records
        .load(Ordering::Relaxed);
    let c = cache_at(staging.path(), data.path(), "mixed-2", Some(&gen_a))
        .await
        .unwrap();
    assert!(
        c.nvme.has_staged_active_block(&mine_key),
        "our own record must be adopted"
    );
    assert!(
        !c.nvme.has_staged_active_block(&foreign_key),
        "a foreign record must never enter the occupancy index"
    );
    assert!(
        c.nvme.list_staged_files().contains(&foreign_key),
        "and must still be present in the ring — never wiped"
    );
    assert!(
        squeezefs::fuse_client::METRICS
            .staging_foreign_scope_records
            .load(Ordering::Relaxed)
            > before,
        "the foreign-record tripwire must count it"
    );
}

/// The W2 mount-time extent-record sweep (design-random-small-writes §5.2)
/// keeps every landed law for OUR records — generation-bound,
/// fencing-stamped (the DLM S2 per-ino currency rule), torn-discarded,
/// future-refused — and adds exactly one arm: a record whose writer scope
/// we cannot claim is left INTACT and counted, never validated, folded,
/// discarded or stamp-judged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extent_sweep_leaves_foreign_records_and_recovers_ours() {
    use squeezefs::cache::nvme::{ExtentRecord, EXTENT_RECORD_VERSION};
    let _g = serial().await;
    let staging = tempdir().unwrap();
    let data = NamedTempFile::new().unwrap();
    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(128 * 1024 * 1024).unwrap();
    std::fs::File::create(data.path())
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    squeezefs::meta_backend::kv::builder::ImageBuilder::new(
        squeezefs::meta_backend::kv::builder::BuilderConfig {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xABCD_1234_5678_9012,
            uuid: [3u8; 16],
        },
    )
    .unwrap()
    .build(meta.path(), 128 * 1024 * 1024)
    .await
    .unwrap();

    let _sg = engage(NODE_A);
    let gen_a = ws::staging_generation("v3:03030303030303030303030303030303", Some(node(NODE_A)));

    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    let cache = cache_at(staging.path(), data.path(), "extsweep", Some(&gen_a))
        .await
        .unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(data.path().to_str().unwrap()));
    let ba = Arc::new(BlockAllocator::new("extsweep-ba").await.unwrap());
    let router = squeezefs::routing::DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = squeezefs::fuse_client::SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let be = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(meta.path())
        .await
        .unwrap();
    let routed = Arc::new(squeezefs::meta_backend::RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);

    let mine = squeezefs::keys::active_block_ext(21, 1).to_string();
    let foreign = format!("active_block_ext:inode_22:block_1{}", suffix(NODE_B));
    let legacy = "active_block_ext:inode_23:block_1".to_string();
    for key in [&mine, &foreign, &legacy] {
        let rec = ExtentRecord {
            version: EXTENT_RECORD_VERSION,
            fencing_token: 0,
            block_idx: 1,
            base_deferred: true,
            extents: vec![(4096, vec![0x5Cu8; 2048])],
        };
        assert!(
            fs.router
                .cache
                .nvme
                .put_active_block(key, &rec.serialize(), rec.fencing_token),
            "seeding {key}"
        );
    }

    let foreign_before = squeezefs::fuse_client::METRICS
        .extent_records_foreign_scope
        .load(Ordering::Relaxed);
    let recovered = fs.recover_extent_records().await;
    assert_eq!(
        recovered, 2,
        "ours + the grandfathered legacy record recover; the foreign one does not"
    );
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .extent_records_foreign_scope
            .load(Ordering::Relaxed),
        foreign_before + 1,
        "the foreign record is counted in its own class"
    );
    assert!(
        fs.router.cache.nvme.has_staged_extent_record(&foreign),
        "and left INTACT — it is another writer's acked custody"
    );
    assert!(fs.router.cache.nvme.has_staged_extent_record(&mine));
    assert!(fs.router.cache.nvme.has_staged_extent_record(&legacy));
}

/// The format seam → engagement wiring: a volume stamped with bit 10
/// engages the scope for the whole set, an un-stamped one does not, and
/// unanimity is required (a half-stamped set is NOT scoped — labelling
/// some records and not others is strictly worse than labelling none).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_scope_resolution_requires_a_unanimously_stamped_set() {
    let _g = serial().await;
    let plain = NamedTempFile::new().unwrap();
    let stamped = NamedTempFile::new().unwrap();
    for f in [&plain, &stamped] {
        f.as_file().set_len(96 * 1024 * 1024).unwrap();
    }
    let opts = squeezefs::meta_backend::kv::builder::FormatV3Options {
        node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    };
    // Single-writer-class bases: the rung-10b DEFAULT format stamps the
    // whole nine-bit set (bit 10 included), so the unscoped arm and the
    // seam's isolated bit-10 arm both build on the explicit opt-out.
    squeezefs::meta_backend::kv::builder::format_v3_single_writer(
        plain.path(),
        96 * 1024 * 1024,
        &opts,
    )
    .await
    .unwrap();
    std::env::set_var("SQUEEZEFS_TEST_STAMP_WRITER_SCOPE", "1");
    squeezefs::meta_backend::kv::builder::format_v3_single_writer(
        stamped.path(),
        96 * 1024 * 1024,
        &opts,
    )
    .await
    .unwrap();
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_WRITER_SCOPE");

    let p = plain.path().to_string_lossy().into_owned();
    let s = stamped.path().to_string_lossy().into_owned();
    assert_eq!(
        ws::resolve_scope_for_set(std::slice::from_ref(&p))
            .await
            .unwrap(),
        None,
        "an un-stamped set is UNSCOPED — exactly today's behavior (ruling D9)"
    );
    let scoped = ws::resolve_scope_for_set(std::slice::from_ref(&s))
        .await
        .unwrap();
    assert!(
        scoped.is_some(),
        "a stamped set engages the node scope (this host has a machine-id)"
    );
    assert_eq!(
        ws::resolve_scope_for_set(&[s, p]).await.unwrap(),
        None,
        "unanimity is required: a half-stamped set stays unscoped"
    );
    // The same verdict from already-open superblock feature words (the
    // fsck/job path) — one law, two entry points.
    assert_eq!(
        ws::scope_for_features([sb::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING]),
        scoped
    );
    assert_eq!(ws::scope_for_features([0u64]), None);
    assert_eq!(ws::scope_for_features(std::iter::empty()), None);
}

// ---------------------------------------------------------------------------
// 6. KD-8 composition (design-volume-lifecycle KD-8) — the data-loss pin
// ---------------------------------------------------------------------------

/// KD-8 rebind under node scoping: the crash-safe two-phase rebind must
/// still adopt at the NEW generation, and every crash prefix must stay
/// adoptable.
///
/// The failure this pins is a DATA-LOSS path: if the barrier compared
/// bare set generations while the mount computes a node-scoped one, the
/// rebind would classify our own root as foreign and leave it bound to the
/// OLD set generation — and the next mount would take the dead-generation
/// discard arm on durable acked staged payloads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kd8_rebind_composes_with_node_scoping() {
    let _g = serial().await;
    let staging = tempdir().unwrap();
    let data = NamedTempFile::new().unwrap();
    let old_set = "v3:6666666666666666666666666666666f";
    let new_set = "v3:6666666666666666666666666666666f|v3:7777777777777777777777777777777f";

    let _sg = engage(NODE_A);
    let old_gen = ws::staging_generation(old_set, Some(node(NODE_A)));
    let new_gen = ws::staging_generation(new_set, Some(node(NODE_A)));

    // A durable staged-layout payload (uuid file id — the class KD-8
    // REBINDS rather than refuses) in a root bound to the old generation.
    {
        let c = cache_at(staging.path(), data.path(), "kd8-a", Some(&old_gen))
            .await
            .unwrap();
        c.nvme
            .stage_write(
                "inode_5",
                "8f14e45fceea167a5a36dedd4bea2543",
                bytes::Bytes::from(vec![0x33u8; 8192]),
                7,
            )
            .await
            .unwrap();
    }

    let dirs = vec![staging.path().to_path_buf()];
    // Phase 1: the dual marker binds BOTH node-scoped generations.
    squeezefs::config_ops::staging_rebind_prepare(&dirs, &old_gen, &new_gen)
        .await
        .expect("prepare must accept our own node-scoped root");
    let dual = marker(staging.path()).unwrap();
    assert_eq!(
        dual,
        format!("squeezefs-staging-generation-v1\n{old_gen}\n{new_gen}\n"),
        "the two-phase marker must bind both node-scoped generations"
    );

    // Crash prefix A (post-prepare, pre-flip): the OLD set still adopts.
    {
        let discards = staging_discards();
        let c = cache_at(staging.path(), data.path(), "kd8-crashA", Some(&old_gen))
            .await
            .unwrap();
        assert_eq!(staging_discards(), discards, "no discard");
        assert!(
            c.nvme
                .read_staged("8f14e45fceea167a5a36dedd4bea2543")
                .is_some(),
            "the durable staged payload must survive the pre-flip prefix"
        );
    }
    // The mount above canonicalized the marker to the OLD generation; a
    // resumed rebind re-marks it (idempotent by design).
    squeezefs::config_ops::staging_rebind_prepare(&dirs, &old_gen, &new_gen)
        .await
        .unwrap();

    // Phase 2 + the NEW set's mount: adopted, payload intact.
    squeezefs::config_ops::staging_rebind_finalize(&dirs, &new_gen)
        .await
        .unwrap();
    assert_eq!(
        marker(staging.path()).unwrap(),
        format!("squeezefs-staging-generation-v1\n{new_gen}\n")
    );
    let discards = staging_discards();
    let c = cache_at(staging.path(), data.path(), "kd8-b", Some(&new_gen))
        .await
        .unwrap();
    assert_eq!(
        staging_discards(),
        discards,
        "a membership change must never discard durable staged payloads"
    );
    assert!(
        c.nvme
            .read_staged("8f14e45fceea167a5a36dedd4bea2543")
            .is_some(),
        "the rebound payload must still be readable after the change"
    );
}

/// The KD-8 data-loss path, end to end: a staging root still bound to the
/// UN-SCOPED old generation (stamped before the set's bit was) carries a
/// durable staged payload across a membership change. If the barrier's
/// membership test rejected that binding, the root would be left on the
/// DEAD old set generation and the next mount would take the
/// dead-generation discard arm on acked staged data.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kd8_rebind_of_an_unscoped_root_never_discards_staged_payloads() {
    let _g = serial().await;
    let staging = tempdir().unwrap();
    let data = NamedTempFile::new().unwrap();
    let old_set = "v3:ccccccccccccccccccccccccccccccdf";
    let new_set = "v3:ccccccccccccccccccccccccccccccdf|v3:ddddddddddddddddddddddddddddddef";
    let file_id = "c4ca4238a0b923820dcc509a6f75849b";

    // Pre-bit binary: unscoped root, durable staged payload.
    ws::engage(None);
    {
        let c = cache_at(staging.path(), data.path(), "kd8u-a", Some(old_set))
            .await
            .unwrap();
        c.nvme
            .stage_write(
                "inode_6",
                file_id,
                bytes::Bytes::from(vec![0x44u8; 8192]),
                7,
            )
            .await
            .unwrap();
    }

    // The set is now stamped: the offline verb rebinds with node-scoped
    // generations, and the root still bound to the bare old generation
    // MUST be carried across.
    let _sg = engage(NODE_A);
    let old_gen = ws::staging_generation(old_set, Some(node(NODE_A)));
    let new_gen = ws::staging_generation(new_set, Some(node(NODE_A)));
    let dirs = vec![staging.path().to_path_buf()];
    squeezefs::config_ops::staging_rebind_prepare(&dirs, &old_gen, &new_gen)
        .await
        .unwrap();
    squeezefs::config_ops::staging_rebind_finalize(&dirs, &new_gen)
        .await
        .unwrap();
    assert_eq!(
        marker(staging.path()).unwrap(),
        format!("squeezefs-staging-generation-v1\n{new_gen}\n"),
        "the un-scoped root must be rebound to the new NODE-SCOPED generation"
    );

    let discards = staging_discards();
    let c = cache_at(staging.path(), data.path(), "kd8u-b", Some(&new_gen))
        .await
        .unwrap();
    assert_eq!(
        staging_discards(),
        discards,
        "a membership change over a pre-bit root must never discard staged payloads"
    );
    assert!(
        c.nvme.read_staged(file_id).is_some(),
        "the durable staged payload must survive the rebind"
    );
}

/// The membership test the KD-8 barrier uses must accept the UN-scoped
/// binding on a scoped set (`ScopeUpgrade`). Skipping it would strand the
/// root on the dead set generation — the discard-on-next-mount data-loss
/// path — so this is pinned as its own law.
#[tokio::test]
async fn kd8_membership_test_accepts_the_unscoped_binding() {
    let set = "v3:8888888888888888888888888888888f";
    let scoped = ws::staging_generation(set, Some(node(NODE_A)));
    assert!(
        ws::marker_is_rebindable(set, &scoped),
        "a bare-marker root MUST be rebindable under a scoped set"
    );
    assert!(ws::marker_is_rebindable(&scoped, &scoped));
    assert!(
        !ws::marker_is_rebindable(&ws::staging_generation(set, Some(node(NODE_B))), &scoped),
        "a peer's root is never ours to rebind"
    );
    assert!(
        !ws::marker_is_rebindable("v3:9999999999999999999999999999999f", &scoped),
        "a dead set is never rebindable"
    );
}

/// KD-8 phase 1 still refuses PENDING write custody (the landed law), and
/// a foreign-scoped custody record refuses too — this process cannot drain
/// custody whose payload ring belongs to another node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kd8_prepare_refuses_pending_and_foreign_custody() {
    let _g = serial().await;
    let staging = tempdir().unwrap();
    let set = "v3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaf";
    let _sg = engage(NODE_A);
    let old_gen = ws::staging_generation(set, Some(node(NODE_A)));
    let new_gen = ws::staging_generation("v3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbf", Some(node(NODE_A)));
    squeezefs::cache::write_staging_generation_marker(staging.path(), &old_gen)
        .await
        .unwrap();

    let foreign = format!("active_block:inode_2:block_0{}", suffix(NODE_B));
    squeezefs::cache::seed_staged_custody_for_test(staging.path(), &foreign)
        .await
        .unwrap();
    let dirs = vec![staging.path().to_path_buf()];
    let err = squeezefs::config_ops::staging_rebind_prepare(&dirs, &old_gen, &new_gen)
        .await
        .expect_err("pending write custody must refuse the barrier");
    let msg = err.to_string();
    assert!(
        msg.contains("staging drain barrier refused") && msg.contains(&foreign),
        "the refusal must name the offending unit: {msg}"
    );
}

// ---------------------------------------------------------------------------
// 7. Bit hygiene (ruling D9)
// ---------------------------------------------------------------------------

/// Every incompat constant is a DISTINCT SINGLE bit, and the known mask is
/// exactly their union. Two agents once claimed bit 8 in parallel — silent
/// on-disk aliasing — so this is a gate, not a convention.
#[test]
fn incompat_bits_are_single_bit_and_pairwise_disjoint() {
    let bits: &[(&str, u64)] = &[
        ("KV_V3", sb::FEATURE_INCOMPAT_KV_V3),
        (
            "NODE_SEQ_WATERMARK",
            sb::FEATURE_INCOMPAT_NODE_SEQ_WATERMARK,
        ),
        ("KV_GUEST_SLOTS", sb::FEATURE_INCOMPAT_KV_GUEST_SLOTS),
        (
            "KV_VOLUME_LIFECYCLE",
            sb::FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE,
        ),
        ("KV_SLOT_MIGRATION", sb::FEATURE_INCOMPAT_KV_SLOT_MIGRATION),
        ("KV_LAYOUT_DELTAS", sb::FEATURE_INCOMPAT_KV_LAYOUT_DELTAS),
        (
            "KV_DYNAMIC_ROUTING",
            sb::FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING,
        ),
        ("KV_DURABLE_TERM", sb::FEATURE_INCOMPAT_KV_DURABLE_TERM),
        (
            "KV_PARTITIONED_APPEND",
            sb::FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
        ),
        (
            "KV_BLOCK_REFCOUNTS",
            sb::FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
        ),
        (
            "KV_WRITER_SCOPED_STAGING",
            sb::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING,
        ),
        // DLM S7's capability gate. It claimed bit 10 in parallel with this
        // branch and renumbered to 11 at integration — the second parallel
        // claim in this program, and the reason THIS assertion exists: the
        // union clause is what turned a silent on-disk aliasing into a red
        // test the moment both landed.
        (
            "KV_MULTI_WRITER_DATA",
            sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        ),
        // §6.2 items 5/6. These two also authored themselves at 10/11 and
        // renumbered to 12/13 at integration — the THIRD parallel claim. The
        // union clause below is what makes that a red test rather than an
        // on-disk aliasing bug discovered in the field.
        ("KV_INO_LANES", sb::FEATURE_INCOMPAT_KV_INO_LANES),
        (
            "KV_BLOCK_KEY_INCARNATION",
            sb::FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION,
        ),
        ("KV_CLAIM_SET", sb::FEATURE_INCOMPAT_KV_CLAIM_SET),
        // §6.2 item 9 (durable per-ino layout versions) took bit 15 — the
        // last free bit below 16, claimed against a program with FIVE
        // prior parallel-claim collisions; this union clause is what
        // makes the sixth a red gate instead of on-disk aliasing.
        (
            "KV_LAYOUT_VERSIONS",
            sb::FEATURE_INCOMPAT_KV_LAYOUT_VERSIONS,
        ),
        // PB-class files (docs/design-kvmap-block-map-tree.md): the
        // block-map tree took bit 16.
        ("KV_BLOCK_MAP_TREE", sb::FEATURE_INCOMPAT_KV_BLOCK_MAP_TREE),
    ];
    let mut seen = 0u64;
    for (name, bit) in bits {
        assert_eq!(bit.count_ones(), 1, "{name} must be a single bit");
        assert_eq!(seen & bit, 0, "{name} ALIASES an earlier bit ({bit:#x})");
        seen |= bit;
    }
    assert_eq!(
        seen,
        sb::FEATURES_INCOMPAT_KNOWN,
        "FEATURES_INCOMPAT_KNOWN must be exactly the union of the constants"
    );
    assert_eq!(
        sb::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING,
        1 << sb::WRITER_SCOPED_STAGING_BIT
    );
    assert_eq!(sb::WRITER_SCOPED_STAGING_BIT, 10, "bit 10 (8 and 9 taken)");
}

/// Ruling D9: `plan()` (i.e. every production `format`) must NOT stamp the
/// bit, and the read ceiling for staging content format must stay above
/// the version an un-scoped mount writes.
#[test]
fn production_format_never_stamps_the_writer_scope_bit() {
    let plan = sb::SuperblockV3::plan(1 << 30, 262_144, None, [7u8; 16], 0x1234).unwrap();
    assert_eq!(
        plan.features_incompat & sb::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING,
        0,
        "a fresh format must mount UNSCOPED (ruling D9: build the bit, do not stamp it)"
    );
    assert_eq!(STAGING_FORMAT_VERSION, 3);
    assert_eq!(staging_format_write_version(false), 2);
    assert_eq!(staging_format_write_version(true), 3);
}

// ---------------------------------------------------------------------------
// 8. KD-MW-2 (design-full-multi-writer §5.1) — the client scope is the PAIR
// `(node_token, mount_slot)`, and the moved-mount-point law (MW-1b)
// ---------------------------------------------------------------------------

const SLOT_A: u32 = 0x00c0_ffee;
const SLOT_B: u32 = 0x0badc0de;

fn pair_suffix(token: u64, slot: u32) -> String {
    format!(":w_{token:016x}.m{slot:08x}")
}

/// Co-located scopes classify APART (the rung-1 red-first pin): two
/// mounts of one node with distinct slots are distinct clients at both
/// grammar levels — keys and staging generations — while the same-node
/// node-only forms stay grandfathered (the pre-pair upgrade arms).
#[tokio::test]
async fn co_located_pair_scopes_classify_apart() {
    let _g = serial().await;
    let key_a = format!(
        "active_block:inode_1:block_0{}",
        pair_suffix(NODE_A, SLOT_A)
    );
    let key_b = format!(
        "active_block:inode_1:block_0{}",
        pair_suffix(NODE_A, SLOT_B)
    );
    let key_node = format!("active_block:inode_1:block_0{}", suffix(NODE_A));
    {
        let _sg = engage_pair(NODE_A, SLOT_A);
        assert_eq!(ws::classify_key(&key_a), ws::KeyOwner::Mine);
        assert_eq!(
            ws::classify_key(&key_b),
            ws::KeyOwner::Foreign(pair(NODE_A, SLOT_B)),
            "a co-located sibling's record is FOREIGN — never adopted"
        );
        assert_eq!(
            ws::classify_key(&key_node),
            ws::KeyOwner::Mine,
            "a pre-pair node-only record is grandfathered ours (same D0 argument)"
        );
        // A foreign NODE stays foreign regardless of slot.
        let key_bn = format!(
            "active_block:inode_1:block_0{}",
            pair_suffix(NODE_B, SLOT_A)
        );
        assert_eq!(
            ws::classify_key(&key_bn),
            ws::KeyOwner::Foreign(pair(NODE_B, SLOT_A))
        );
        // Strip is exact for the pair form too.
        assert_eq!(ws::strip_key_scope(&key_a), "active_block:inode_1:block_0");
    }
    // A node-only identity (the OFFLINE processes) cannot claim a slotted
    // record: it cannot prove WHICH mount's record it is.
    {
        let _sg = engage(NODE_A);
        assert_eq!(
            ws::classify_key(&key_a),
            ws::KeyOwner::Foreign(pair(NODE_A, SLOT_A))
        );
    }

    // The generation table, pair arms.
    let set = "v3:1212121212121212121212121212121f";
    let gen_a = ws::staging_generation(set, Some(pair(NODE_A, SLOT_A)));
    let gen_b = ws::staging_generation(set, Some(pair(NODE_A, SLOT_B)));
    let gen_node = ws::staging_generation(set, Some(node(NODE_A)));
    assert_eq!(
        ws::classify_generation(&gen_a, &gen_a, Some(pair(NODE_A, SLOT_A))),
        ws::GenerationBinding::Match
    );
    assert_eq!(
        ws::classify_generation(&gen_b, &gen_a, Some(pair(NODE_A, SLOT_A))),
        ws::GenerationBinding::ForeignScope(pair(NODE_A, SLOT_B)),
        "a co-located sibling's ROOT is foreign — never adopted, never wiped"
    );
    assert_eq!(
        ws::classify_generation(&gen_node, &gen_a, Some(pair(NODE_A, SLOT_A))),
        ws::GenerationBinding::ScopeUpgrade,
        "a pre-pair node-only root upgrades (adopt + re-stamp)"
    );
    assert_eq!(
        ws::classify_generation(&gen_a, &gen_node, Some(node(NODE_A))),
        ws::GenerationBinding::ScopeUpgrade,
        "the OFFLINE node-only identity may rebind this node's slotted roots \
         (the KD-8 data-loss-avoidance direction)"
    );
    assert_eq!(
        ws::classify_generation(
            &ws::staging_generation(set, Some(pair(NODE_B, SLOT_A))),
            &gen_a,
            Some(pair(NODE_A, SLOT_A))
        ),
        ws::GenerationBinding::ForeignScope(pair(NODE_B, SLOT_A)),
        "another node's slotted root is foreign regardless of slot"
    );
}

/// Stack and heap mints agree under a PAIR scope, and the pair suffix is
/// what every scoped key carries (the item-8 grammar, KD-MW-2 form).
#[tokio::test]
async fn pair_scope_mints_agree_and_carry_the_pair() {
    let _g = serial().await;
    let _sg = engage_pair(NODE_A, SLOT_A);
    let s = pair_suffix(NODE_A, SLOT_A);
    assert_eq!(
        &*squeezefs::keys::active_block(7, 3),
        format!("active_block:inode_7:block_3{s}")
    );
    assert_eq!(
        &*squeezefs::keys::active_block_stack(7, 3),
        &*squeezefs::keys::active_block(7, 3)
    );
    assert_eq!(
        &*squeezefs::keys::active_block_ext_path_stack("inode_7", 3).unwrap(),
        &*squeezefs::keys::active_block_ext_for_path("inode_7", 3)
    );
    assert_eq!(&*squeezefs::keys::mapping("fid"), format!("mapping:fid{s}"));
}

/// Co-located records in ONE ring are never cross-adopted (the rung-1
/// red-first pin at the ring level): the sibling slot's record is not
/// occupancy-indexed, not budget-counted, and left intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn co_located_records_are_never_cross_adopted() {
    let _g = serial().await;
    let staging = tempdir().unwrap();
    let data = NamedTempFile::new().unwrap();
    let set = "v3:1313131313131313131313131313131f";
    let gen_a = ws::staging_generation(set, Some(pair(NODE_A, SLOT_A)));
    let mine = format!(
        "active_block:inode_8:block_1{}",
        pair_suffix(NODE_A, SLOT_A)
    );
    let sibling = format!(
        "active_block:inode_8:block_0{}",
        pair_suffix(NODE_A, SLOT_B)
    );

    let _sg = engage_pair(NODE_A, SLOT_A);
    {
        let c = cache_at(staging.path(), data.path(), "colo-1", Some(&gen_a))
            .await
            .unwrap();
        assert!(c
            .nvme
            .put_active_block(&mine, &vec![0x31u8; BS as usize], 7));
        assert!(c
            .nvme
            .put_active_block(&sibling, &vec![0x32u8; BS as usize], 7));
    }
    let c = cache_at(staging.path(), data.path(), "colo-2", Some(&gen_a))
        .await
        .unwrap();
    assert!(c.nvme.has_staged_active_block(&mine), "ours adopts");
    assert!(
        !c.nvme.has_staged_active_block(&sibling),
        "a co-located sibling's record must never enter the occupancy index"
    );
    assert!(
        c.nvme.list_staged_files().contains(&sibling),
        "and must still be present in the ring — never wiped"
    );
}

/// The crash-recovery continuity law under pairs (rung-1 red-first pin):
/// a successor mount of the SAME mount point — the same pair — adopts its
/// predecessor's residue exactly as before (Match, no discard).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_mount_point_successor_adopts_its_own_pair_residue() {
    let _g = serial().await;
    let staging = tempdir().unwrap();
    let data = NamedTempFile::new().unwrap();
    let set = "v3:1414141414141414141414141414141f";
    let gen = ws::staging_generation(set, Some(pair(NODE_A, SLOT_A)));

    let _sg = engage_pair(NODE_A, SLOT_A);
    {
        let c = cache_at(staging.path(), data.path(), "succ-1", Some(&gen))
            .await
            .unwrap();
        assert!(c.nvme.put_active_block(
            &squeezefs::keys::active_block(9, 0),
            &vec![0x41u8; BS as usize],
            7
        ));
    }
    let discards = staging_discards();
    let c = cache_at(staging.path(), data.path(), "succ-2", Some(&gen))
        .await
        .unwrap();
    assert_eq!(staging_discards(), discards, "no discard on a warm restart");
    assert!(
        c.nvme
            .has_staged_active_block(&squeezefs::keys::active_block(9, 0)),
        "the same mount point's successor adopts its own residue"
    );
    // The slot derivation is what makes "same mount point" mean "same
    // client": stable across restarts, distinct across paths.
    assert_eq!(
        ws::derive_mount_slot("/mnt/x"),
        ws::derive_mount_slot("/mnt/x")
    );
    assert_ne!(
        ws::derive_mount_slot("/mnt/x"),
        ws::derive_mount_slot("/mnt/y")
    );
}

// ---------------------------------------------------------------------------
// 9. MW-1b — the moved-mount-point law, end to end (report / adopt /
// discard / collision), over the config_ops machinery the mount wires in
// ---------------------------------------------------------------------------

/// A minimal-but-complete format config (the `base_format_config` fixture
/// shape the fsck suites use — `FormatConfig` has no serde defaults).
fn mw1b_format_config() -> squeezefs::FormatConfig {
    squeezefs::FormatConfig {
        name: "squeezefs".to_string(),
        block_size: BS,
        capacity: 1 << 30,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: None,
        data_volumes: None,
        read_cache_size: None,
        write_cache_size: None,
        read_mem_cache_size: None,
        write_mem_cache_size: None,
        dismount_wait: None,
        upload_delay: None,
        fuse_io_uring_sqpoll_idle_ms: None,
        meta_routing_width: None,
        meta_slot_runs: None,
        meta_volumes: None,
    }
}

/// Everything the MW-1b fixtures need: a bit-10-stamped meta volume whose
/// format config declares `cache_root` as its staging path, a pinned node
/// identity, and the set generation.
struct Mw1bFixture {
    _dir: tempfile::TempDir,
    meta: NamedTempFile,
    container: std::path::PathBuf,
    node: u64,
    set: String,
    saved_node_id_file: Option<String>,
}

impl Mw1bFixture {
    async fn new() -> Self {
        let dir = tempdir().unwrap();
        let cache_root = dir.path().join("cache");
        let container = cache_root.join("squeezefs");
        std::fs::create_dir_all(&container).unwrap();

        // Pin the node identity through the env seam (deterministic and
        // hermetic — the verbs resolve it themselves).
        let id_file = dir.path().join("node-id");
        std::fs::write(&id_file, "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a\n").unwrap();
        let saved = std::env::var("SQUEEZEFS_NODE_ID_FILE").ok();
        std::env::set_var("SQUEEZEFS_NODE_ID_FILE", &id_file);
        let node = ws::resolve_node_identity().expect("pinned identity").token;

        let meta = NamedTempFile::new().unwrap();
        meta.as_file().set_len(96 * 1024 * 1024).unwrap();
        let mut cfg = mw1b_format_config();
        cfg.disk_cache_paths = Some(vec![cache_root.clone()]);
        std::env::set_var("SQUEEZEFS_TEST_STAMP_WRITER_SCOPE", "1");
        squeezefs::meta_backend::kv::builder::format_v3(
            meta.path(),
            96 * 1024 * 1024,
            &squeezefs::meta_backend::kv::builder::FormatV3Options {
                node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
                journal_len_override: None,
                force: true,
                full_wipe: false,
                format_config_xattr: Some(serde_json::to_vec(&cfg).unwrap()),
            },
        )
        .await
        .unwrap();
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_WRITER_SCOPE");
        let meta_lvs = vec![meta.path().to_string_lossy().into_owned()];
        let set = squeezefs::meta_backend::volume_set_generation(&meta_lvs)
            .await
            .unwrap();
        Self {
            _dir: dir,
            meta,
            container,
            node,
            set,
            saved_node_id_file: saved,
        }
    }

    fn meta_lvs(&self) -> Vec<String> {
        vec![self.meta.path().to_string_lossy().into_owned()]
    }

    /// Mint a residue staging root bound to `(node, slot)` carrying one
    /// live staged custody record under `key`.
    async fn seed_residue(&self, dirname: &str, slot: u32, key: &str) -> std::path::PathBuf {
        let dir = self.container.join(dirname);
        std::fs::create_dir_all(&dir).unwrap();
        squeezefs::cache::write_staging_generation_marker(
            &dir,
            &ws::staging_generation(&self.set, Some(pair(self.node, slot))),
        )
        .await
        .unwrap();
        squeezefs::cache::seed_staged_custody_for_test(&dir, key)
            .await
            .unwrap();
        dir
    }
}

impl Drop for Mw1bFixture {
    fn drop(&mut self) {
        squeezefs::config_ops::release_staging_root_locks();
        match self.saved_node_id_file.take() {
            Some(v) => std::env::set_var("SQUEEZEFS_NODE_ID_FILE", v),
            None => std::env::remove_var("SQUEEZEFS_NODE_ID_FILE"),
        }
    }
}

/// MW-1b (the rung-1 red-first pin): moved-path residue is reported on
/// EVERY mount until resolved; `staging discard` destroys exactly the
/// reported residue with its keys enumerated; afterwards the report is
/// clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moved_mount_point_residue_is_reported_until_discarded() {
    let _g = serial().await;
    let fx = Mw1bFixture::new().await;
    let residue_key = format!(
        "active_block:inode_2:block_0{}",
        pair_suffix(fx.node, SLOT_A)
    );
    let residue_dir = fx.seed_residue("mnt_old", SLOT_A, &residue_key).await;
    let own_dir = fx.container.join("mnt_new");
    std::fs::create_dir_all(&own_dir).unwrap();
    let our_gen = ws::staging_generation(&fx.set, Some(pair(fx.node, SLOT_B)));

    // Mount 1: the residue is detected and the report names the slot and
    // BOTH remedies.
    let scan = squeezefs::config_ops::mount_scoped_staging_prelude(
        std::slice::from_ref(&fx.container),
        std::slice::from_ref(&own_dir),
        "/mnt/new",
        &our_gen,
    )
    .await
    .expect("the mount proceeds — the residue is not ours to touch");
    assert!(scan.adopted.is_empty(), "a foreign slot is never adopted");
    assert_eq!(scan.residue.len(), 1, "the residue must be detected");
    assert_eq!(scan.residue[0].scope, pair(fx.node, SLOT_A));
    let report =
        squeezefs::config_ops::residue_report(&scan.residue).expect("residue must render a report");
    assert!(report.contains("MOVED-MOUNT-POINT"), "{report}");
    assert!(
        report.contains(&format!("client_slot={SLOT_A:08x}")),
        "the report names the -o client_slot remedy: {report}"
    );
    assert!(
        report.contains("remount at the original path"),
        "the report names the design's remedy string: {report}"
    );
    assert!(
        report.contains("staging adopt") && report.contains("staging discard"),
        "the report names both verbs: {report}"
    );
    squeezefs::config_ops::release_staging_root_locks();

    // Mount 2: STILL reported (never silently stranded, never auto-fixed).
    let scan = squeezefs::config_ops::mount_scoped_staging_prelude(
        std::slice::from_ref(&fx.container),
        std::slice::from_ref(&own_dir),
        "/mnt/new",
        &our_gen,
    )
    .await
    .unwrap();
    assert_eq!(
        scan.residue.len(),
        1,
        "the report repeats on every mount until resolved"
    );
    squeezefs::config_ops::release_staging_root_locks();

    // `squeezefs clients` renders the residue slot (the listing an
    // operator copies the hex8 from).
    let listed = squeezefs::config_ops::slot_residue_for_set(&fx.meta_lvs())
        .await
        .unwrap();
    assert!(
        listed
            .iter()
            .any(|r| r.scope == pair(fx.node, SLOT_A) && !r.live_owner),
        "clients must list the dead residue slot: {listed:?}"
    );

    // Discard destroys it, enumerating the freed keys (attested verb).
    let (dirs, keys) = squeezefs::config_ops::staging_discard(&fx.meta_lvs(), SLOT_A)
        .await
        .expect("discard resolves the residue");
    assert_eq!(dirs, vec![residue_dir.clone()]);
    assert!(
        keys.contains(&residue_key),
        "the destroyed custody keys are enumerated: {keys:?}"
    );
    assert!(!residue_dir.exists(), "the residue root is destroyed");

    // Mount 3: clean.
    let scan = squeezefs::config_ops::mount_scoped_staging_prelude(
        std::slice::from_ref(&fx.container),
        std::slice::from_ref(&own_dir),
        "/mnt/new",
        &our_gen,
    )
    .await
    .unwrap();
    assert!(
        scan.residue.is_empty(),
        "after discard the report must be clean"
    );
}

/// `staging adopt` re-binds durable residue to the node (KD-8 two-phase),
/// the NEXT mount binds it (tombstone + liveness-lock winner-takes), and
/// pending BLOCK custody refuses the verb naming the `-o client_slot`
/// drain remedy (KD-8's own law, carried over).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_adopt_rebinds_durable_residue_for_the_next_mount() {
    let _g = serial().await;
    let fx = Mw1bFixture::new().await;
    // Durable staged payload: a uuid file-id record (UNSCOPED key — the
    // class KD-8 rebinds rather than refuses).
    let residue_dir = fx
        .seed_residue("mnt_gone", SLOT_A, "8f14e45fceea167a5a36dedd4bea2543")
        .await;

    let adopted = squeezefs::config_ops::staging_adopt(&fx.meta_lvs(), SLOT_A)
        .await
        .expect("durable residue adopts");
    assert_eq!(adopted, vec![residue_dir.clone()]);
    assert_eq!(
        std::fs::read_to_string(residue_dir.join(STAGING_GENERATION_MARKER)).unwrap(),
        format!(
            "squeezefs-staging-generation-v1\n{}\n",
            ws::staging_generation(&fx.set, Some(node(fx.node)))
        ),
        "the root is re-bound to the invoking identity — the NODE"
    );
    assert!(
        residue_dir
            .join(squeezefs::config_ops::STAGING_NODE_ADOPTED_MARKER)
            .exists(),
        "the adopt tombstone authorizes the next mount to bind it"
    );

    // The residue listing no longer names the slot (it is node-bound now).
    let listed = squeezefs::config_ops::slot_residue_for_set(&fx.meta_lvs())
        .await
        .unwrap();
    assert!(listed.is_empty(), "adopted residue leaves the slot listing");

    // The next scoped mount of this set on this node BINDS it.
    let own_dir = fx.container.join("mnt_new");
    std::fs::create_dir_all(&own_dir).unwrap();
    let our_gen = ws::staging_generation(&fx.set, Some(pair(fx.node, SLOT_B)));
    let scan = squeezefs::config_ops::mount_scoped_staging_prelude(
        std::slice::from_ref(&fx.container),
        std::slice::from_ref(&own_dir),
        "/mnt/new",
        &our_gen,
    )
    .await
    .unwrap();
    assert_eq!(
        scan.adopted,
        vec![residue_dir.clone()],
        "the verb-adopted root is bound by the next mount"
    );
    assert!(
        !residue_dir
            .join(squeezefs::config_ops::STAGING_NODE_ADOPTED_MARKER)
            .exists(),
        "binding consumes the tombstone"
    );
    squeezefs::config_ops::release_staging_root_locks();

    // Pending BLOCK custody refuses the verb (KD-8's law) and names the
    // drain remedy.
    let blocked_key = format!(
        "active_block:inode_3:block_0{}",
        pair_suffix(fx.node, SLOT_B)
    );
    fx.seed_residue("mnt_blocked", SLOT_B, &blocked_key).await;
    let err = squeezefs::config_ops::staging_adopt(&fx.meta_lvs(), SLOT_B)
        .await
        .expect_err("pending write custody refuses the rebind")
        .to_string();
    assert!(
        err.contains(&format!("client_slot={SLOT_B:08x}")),
        "the refusal names the -o client_slot drain remedy: {err}"
    );
}

/// The `-o client_slot=<hex8>` adoption arm: a mount presenting the
/// residue's exact pair BINDS the residue root (its marker is a Match) and
/// reports nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_slot_override_adopts_the_exact_pair_residue() {
    let _g = serial().await;
    let fx = Mw1bFixture::new().await;
    let residue_key = format!(
        "active_block:inode_4:block_0{}",
        pair_suffix(fx.node, SLOT_A)
    );
    let residue_dir = fx.seed_residue("mnt_before", SLOT_A, &residue_key).await;
    let own_dir = fx.container.join("mnt_after");
    std::fs::create_dir_all(&own_dir).unwrap();

    // The override makes OUR pair the residue's pair.
    let our_gen = ws::staging_generation(&fx.set, Some(pair(fx.node, SLOT_A)));
    let scan = squeezefs::config_ops::mount_scoped_staging_prelude(
        std::slice::from_ref(&fx.container),
        std::slice::from_ref(&own_dir),
        "/mnt/after",
        &our_gen,
    )
    .await
    .unwrap();
    assert_eq!(
        scan.adopted,
        vec![residue_dir],
        "an exact-pair dead sibling is ADOPTED (the client_slot remedy)"
    );
    assert!(scan.residue.is_empty(), "adopted residue is not reported");
    squeezefs::config_ops::release_staging_root_locks();
}

/// OQ-5's resolved form, the LOCAL observation arm: an exact-pair sibling
/// root owned by a LIVE mount refuses the mount loud, naming the collision
/// class and the `-o client_slot=` remedy. The verbs refuse a live owner's
/// root too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_pair_collision_refuses_the_mount_and_the_verbs() {
    let _g = serial().await;
    let fx = Mw1bFixture::new().await;
    let residue_key = format!(
        "active_block:inode_5:block_0{}",
        pair_suffix(fx.node, SLOT_A)
    );
    let sibling_dir = fx.seed_residue("mnt_live", SLOT_A, &residue_key).await;
    // Simulate the live sibling: hold its liveness lock.
    squeezefs::config_ops::hold_staging_root_lock(&sibling_dir).unwrap();

    let own_dir = fx.container.join("mnt_second");
    std::fs::create_dir_all(&own_dir).unwrap();
    let our_gen = ws::staging_generation(&fx.set, Some(pair(fx.node, SLOT_A)));
    let err = squeezefs::config_ops::mount_scoped_staging_prelude(
        std::slice::from_ref(&fx.container),
        std::slice::from_ref(&own_dir),
        "/mnt/second",
        &our_gen,
    )
    .await
    .expect_err("two LIVE mounts must never share one client identity")
    .to_string();
    assert!(err.contains("MOUNT-SLOT COLLISION"), "{err}");
    assert!(
        err.contains("client_slot"),
        "the refusal names the remedy: {err}"
    );
    assert!(err.contains("/mnt/second"), "names our mount point: {err}");
    squeezefs::config_ops::release_staging_root_locks();

    // The verbs refuse a live owner's root as well.
    squeezefs::config_ops::hold_staging_root_lock(&sibling_dir).unwrap();
    let err = squeezefs::config_ops::staging_discard(&fx.meta_lvs(), SLOT_A)
        .await
        .expect_err("a live owner's root is never destroyed")
        .to_string();
    assert!(err.contains("LIVE"), "{err}");
    let err = squeezefs::config_ops::staging_adopt(&fx.meta_lvs(), SLOT_A)
        .await
        .expect_err("a live owner's root is never re-bound")
        .to_string();
    assert!(err.contains("LIVE"), "{err}");
}

/// The solo-dark pin (rung 1 "ships DARK"): an UN-scoped staging
/// generation makes the whole mount prelude a structural no-op — no
/// liveness lock file is minted, nothing is adopted, nothing is reported
/// — so an un-stamped solo mount's staging tree is byte-identical to
/// prior releases (the residue machinery only ever engages on
/// pair-decorated generations, which only bit-10 mounts mint).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unscoped_mount_prelude_is_a_structural_noop() {
    let _g = serial().await;
    let dir = tempdir().unwrap();
    let container = dir.path().join("squeezefs");
    let own = container.join("mnt_solo");
    let sibling = container.join("mnt_other");
    std::fs::create_dir_all(&own).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    let set = "v3:2323232323232323232323232323232f";
    squeezefs::cache::write_staging_generation_marker(&sibling, set)
        .await
        .unwrap();

    let scan = squeezefs::config_ops::mount_scoped_staging_prelude(
        std::slice::from_ref(&container),
        std::slice::from_ref(&own),
        "/mnt/solo",
        set, // UN-decorated: the solo posture
    )
    .await
    .unwrap();
    assert!(scan.adopted.is_empty() && scan.residue.is_empty());
    assert!(
        !own.join(squeezefs::config_ops::STAGING_OWNER_LOCK).exists(),
        "an un-scoped mount mints NO liveness lock file — its staging tree \
         is byte-identical to prior releases"
    );
    assert!(
        !sibling
            .join(squeezefs::config_ops::STAGING_OWNER_LOCK)
            .exists(),
        "and it never touches a sibling dir"
    );
}

/// The verbs are honest about scope: an un-scoped volume set carries no
/// mount-slot residue, and the verbs refuse rather than scanning for
/// something that cannot exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_verbs_refuse_an_unscoped_set() {
    let _g = serial().await;
    let meta = NamedTempFile::new().unwrap();
    meta.as_file().set_len(96 * 1024 * 1024).unwrap();
    // The unstamped (single-writer) class — the rung-10b default stamps
    // bit 10, which would make this set SCOPED.
    squeezefs::meta_backend::kv::builder::format_v3_single_writer(
        meta.path(),
        96 * 1024 * 1024,
        &squeezefs::meta_backend::kv::builder::FormatV3Options {
            node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
            journal_len_override: None,
            force: true,
            full_wipe: false,
            format_config_xattr: Some(serde_json::to_vec(&mw1b_format_config()).unwrap()),
        },
    )
    .await
    .unwrap();
    let meta_lvs = vec![meta.path().to_string_lossy().into_owned()];
    let err = squeezefs::config_ops::staging_discard(&meta_lvs, SLOT_A)
        .await
        .expect_err("an un-scoped set carries no slot residue")
        .to_string();
    assert!(
        err.contains("not writer-scoped"),
        "the refusal explains why: {err}"
    );
}

/// The fstests zero-dwell remount race (2026-08-16 — the stamped-solo
/// QUICK gate's first live catch, generic/003 abort): `umount(8)` returns
/// when the kernel FUSE connection closes, but the predecessor daemon's
/// staging-root liveness flock releases only at PROCESS EXIT — so a
/// successor mount at the SAME mount point can meet its OWN root held by
/// a holder that will be gone in milliseconds. A dying holder gets the
/// bounded wait-out (the `mount_writer_guard_tests::
/// test_reopen_waits_out_same_process_teardown_pin` precedent, applied to
/// the staging plane); the mount proceeds, never a refusal. Unstamped
/// mounts never run the prelude, which is why only the stamped posture
/// ever saw this.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_root_flock_race_with_dying_predecessor_is_absorbed() {
    let _g = serial().await;
    let fx = Mw1bFixture::new().await;
    let own_dir = fx.container.join("mnt_racy");
    std::fs::create_dir_all(&own_dir).unwrap();
    let our_gen = ws::staging_generation(&fx.set, Some(pair(fx.node, SLOT_A)));

    // The dying predecessor: a second open-file-description holds the
    // flock and releases it ~300 ms in — far past any one-shot LOCK_NB
    // try, well inside the wait bound.
    let lock_path = own_dir.join(squeezefs::config_ops::STAGING_OWNER_LOCK);
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&holder), libc::LOCK_EX) },
        0
    );
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(holder);
    });

    let started = std::time::Instant::now();
    let scan = squeezefs::config_ops::mount_scoped_staging_prelude(
        std::slice::from_ref(&fx.container),
        std::slice::from_ref(&own_dir),
        "/mnt/racy",
        &our_gen,
    )
    .await
    .expect("a dying predecessor's flock is absorbed, never refused");
    let waited = started.elapsed();
    assert!(scan.adopted.is_empty() && scan.residue.is_empty());
    assert!(
        waited >= std::time::Duration::from_millis(250),
        "the prelude must actually have waited the holder out (waited {waited:?})"
    );
    assert!(
        waited < std::time::Duration::from_secs(2),
        "a freed flock is taken promptly, not at the bound (waited {waited:?})"
    );
    release.join().unwrap();
    squeezefs::config_ops::release_staging_root_locks();
}

/// The posture half: a holder that NEVER releases is a genuinely live
/// co-located mount collision — after the bounded wait expires the loud
/// refusal is unchanged (message verbatim: names the root and the held
/// flock). The wait-out absorbs teardown races; it never weakens the
/// collision refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_root_flock_held_by_live_holder_still_refuses_loud() {
    let _g = serial().await;
    let fx = Mw1bFixture::new().await;
    let own_dir = fx.container.join("mnt_live");
    std::fs::create_dir_all(&own_dir).unwrap();
    let our_gen = ws::staging_generation(&fx.set, Some(pair(fx.node, SLOT_A)));

    let lock_path = own_dir.join(squeezefs::config_ops::STAGING_OWNER_LOCK);
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&holder), libc::LOCK_EX) },
        0
    );

    let started = std::time::Instant::now();
    let err = squeezefs::config_ops::mount_scoped_staging_prelude(
        std::slice::from_ref(&fx.container),
        std::slice::from_ref(&own_dir),
        "/mnt/live",
        &our_gen,
    )
    .await
    .expect_err("a live holder must still refuse")
    .to_string();
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(2),
        "the refusal comes only after the full wait bound"
    );
    assert!(
        err.contains("HELD by a live process") && err.contains("unmount it first"),
        "the refusal text is unchanged: {err}"
    );
    drop(holder);
    squeezefs::config_ops::release_staging_root_locks();
}
