//! PR VL5b — the slot migration engine, red-first
//! (docs/design-volume-lifecycle.md §5.5.2/§5.5.2a/§5.5.2b, KD-7/KD-8,
//! G-VL-4 migration clauses; VL5a's representation contracts stay pinned
//! in `tests/meta_slot_tests.rs`).
//!
//! Contracts pinned here BEFORE any implementation:
//!
//! - **Guest keyspaces** (§5.5.1 adapted — see the deviation note on
//!   `squeezefs::meta_backend::GUEST_NS_SHIFT`): a hosted guest slot's
//!   records live in a disjoint per-slot ino-namespace partition of the
//!   host's three trees (the journal tag byte reserves only a nibble for
//!   tree ids — `kv/journal.rs tag_for` — so per-slot TREE IDS would be a
//!   journal wire-format change; the ino-space partition delivers the
//!   same isolation under the same crash contract). Non-participating
//!   volumes stay byte-identical.
//! - **`KV_SLOT_MIGRATION` = incompat bit 4** (KD-14 extended): the VL5b
//!   stamp extension (explicit `native_slot` + per-slot ino cursors)
//!   fails the VL5a decoder's length equation and would decode-as-absent
//!   — so a NEW bit, unknown to the VL5a mask (bits 0|1|2|3), must gate
//!   it loud (bit-before-first-extended-record ordering).
//! - **Per-slot ino cursors** ride the A/B root ledger (the loom-modeled
//!   `slot_cursor_core` publishes them; guest minting draws from them);
//!   a migrated slot's cursor travels with the slot, so re-minting can
//!   never collide with migrated locals.
//! - **Bulk copy + conveyor pass-task key tee** (§5.5.2): concurrent
//!   commits to the migrating slot are captured as KEYS in a bounded
//!   side log; VALUES are re-read at delta apply; overflow flips the
//!   round to a fresh full snapshot; three consecutive overflows abort
//!   loud (`meta_slot_delta_overflows`).
//! - **Cutover gate** (§5.5.2a): a PER-SLOT admission check at
//!   routed-meta-backend op entry BEFORE any 4a `lock_many` — a parked
//!   op holds ZERO DLM guards and zero node locks; gate parks are tagged
//!   (`meta_slot_gate_parked_commits`) and NEVER escalate to
//!   `disabled_volumes`; the deterministic cross-slot-rename hook proves
//!   it (rename parked, 4a stripes provably free, no escalation, the
//!   rename's legs captured by the delta).
//! - **The §5.5.2b flip protocol**: target-first ordered stamp writes;
//!   crash windows after writes 0/1/2/3 and a torn ledger slot land in
//!   the table's states; per-slot highest-epoch-wins discovery resolves
//!   them; re-run converges; tree-diff equivalence = ∅ end to end.
//! - **KD-8 staging drain barrier**: membership changes verify staging
//!   custody empty (loud per-unit diagnostics, abortable, never a drop)
//!   then RESTAMP the new generation — the discard-on-mismatch law is a
//!   provable no-op.
//! - **add-meta / remove-meta** (offline D0-guarded coordinator posture,
//!   the VL4 `remove_data_volume_offline` shape): ino stability across
//!   the change; victim tombstones refuse loud; survivors-first /
//!   victim-last epoch completeness.

use squeezefs::meta_backend::kv::checkpoint::{
    LedgerRecord, MembershipStamp, ROOT_LEDGER_SLOT_LEN,
};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, FEATURES_INCOMPAT_KNOWN, FEATURE_INCOMPAT_KV_GUEST_SLOTS,
    FEATURE_INCOMPAT_KV_SLOT_MIGRATION, FEATURE_INCOMPAT_KV_V3,
    FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE, FEATURE_INCOMPAT_NODE_SEQ_WATERMARK,
};
use squeezefs::meta_backend::slot_migration::{
    migrate_slot, set_logical_digest, slot_logical_digest, MigrationOptions, MigrationPhase,
    MigrationTestHooks, PhaseHold,
};
use squeezefs::meta_backend::{
    discover_meta_set, guest_local_ino, open_routed_meta_set, plan_meta_slot_set, route_ino_width,
    split_guest_local, volume_set_generation, Metadata, RoutedMetaBackend, GUEST_NS_BASE,
    GUEST_NS_SHIFT,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const VOL_LEN: u64 = 256 * 1024 * 1024;

fn make_file(dir: &Path, name: &str, len: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p
}

fn opts() -> squeezefs::meta_backend::kv::builder::FormatV3Options {
    squeezefs::meta_backend::kv::builder::FormatV3Options {
        node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// Format a stamped set the way `format --meta-slots W` does.
async fn format_stamped_set(metas: &[PathBuf], width: u32) {
    let plan = plan_meta_slot_set(metas.len(), width).expect("plan admits the bounds");
    for (i, m) in metas.iter().enumerate() {
        squeezefs::meta_backend::kv::builder::format_v3_stamped(
            m,
            VOL_LEN,
            &opts(),
            plan.stamps[i].clone(),
        )
        .await
        .expect("format stamped meta volume");
    }
}

fn uris(metas: &[PathBuf]) -> Vec<String> {
    metas.iter().map(|p| p.display().to_string()).collect()
}

async fn shutdown_routed(routed: &Arc<RoutedMetaBackend>) {
    for vol in &routed.volumes {
        vol.shutdown().await.unwrap();
    }
}

/// Populate the set with a mixed dataset: files under root (slot-0
/// keyspace), directories striped across volumes with files + xattrs
/// inside them (populating every volume's native slot). Returns
/// `(name → ino)` for stability assertions.
async fn populate(
    routed: &Arc<RoutedMetaBackend>,
    dirs: usize,
    files: usize,
) -> Vec<(String, u64)> {
    let mut made = Vec::new();
    for i in 0..files {
        let name = format!("rootf{i}");
        let ino = routed
            .create(1, &name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create root file")
            .ino;
        made.push((name, ino));
    }
    for d in 0..dirs {
        let dname = format!("dir{d}");
        let dino = routed
            .create(1, &dname, libc::S_IFDIR | 0o755, 1000, 1000)
            .await
            .expect("mkdir")
            .ino;
        made.push((dname.clone(), dino));
        for i in 0..files {
            let fname = format!("{dname}/f{i}");
            let ino = routed
                .create(dino, &format!("f{i}"), libc::S_IFREG | 0o644, 1000, 1000)
                .await
                .expect("create nested file")
                .ino;
            routed
                .setxattr(ino, "user.tag", fname.as_bytes())
                .await
                .expect("setxattr");
            made.push((fname, ino));
        }
    }
    made
}

/// Resolve every populated name and pin ino + xattr identity.
async fn verify_population(routed: &Arc<RoutedMetaBackend>, made: &[(String, u64)]) {
    for (name, ino) in made {
        let (parent, leaf) = match name.split_once('/') {
            Some((d, f)) => {
                let dino = routed.lookup(1, d).await.expect("dir resolves").ino;
                (dino, f.to_string())
            }
            None => (1u64, name.clone()),
        };
        let got = routed
            .lookup(parent, &leaf)
            .await
            .unwrap_or_else(|e| panic!("lookup {name} failed after migration: {e}"));
        assert_eq!(
            got.ino, *ino,
            "global ino of {name} must be eternally stable"
        );
        if name.contains('/') {
            let tag = routed
                .getxattr(*ino, "user.tag")
                .await
                .expect("getxattr")
                .expect("xattr present");
            assert_eq!(tag, name.as_bytes(), "xattr of {name} intact");
        }
    }
}

// ---------------------------------------------------------------------------
// KD-14 extended: KV_SLOT_MIGRATION = bit 4, unknown to the VL5a mask
// ---------------------------------------------------------------------------

#[test]
fn test_slot_migration_bit_is_bit4_and_vl5a_mask_refuses() {
    assert_eq!(FEATURE_INCOMPAT_KV_SLOT_MIGRATION, 1 << 4, "bit 4");
    let vl5a_mask = FEATURE_INCOMPAT_KV_V3
        | FEATURE_INCOMPAT_NODE_SEQ_WATERMARK
        | FEATURE_INCOMPAT_KV_GUEST_SLOTS
        | FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE;
    assert_eq!(
        FEATURE_INCOMPAT_KV_SLOT_MIGRATION & vl5a_mask,
        0,
        "bit 4 must not intersect the VL5a known mask (bits 0..=3)"
    );
    assert_ne!(
        FEATURES_INCOMPAT_KNOWN & FEATURE_INCOMPAT_KV_SLOT_MIGRATION,
        0,
        "this binary must understand bit 4"
    );
}

/// The bit-before-first-extended-record ordering, one generation later:
/// a cursor-extended stamp fails the VL5a decoder's length equation and
/// decodes as absent (silent ledger fallback) — which is why bit 4 must
/// refuse VL5a-mask binaries at the superblock gate first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bit4_gates_extended_stamps_against_vl5a_binaries() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", VOL_LEN);
    format_stamped_set(std::slice::from_ref(&meta), 1).await;
    squeezefs::meta_backend::kv::superblock::set_slot_migration_bit(&meta)
        .await
        .expect("set bit 4");
    let mut sector = vec![0u8; 4096];
    use std::io::Read;
    std::fs::File::open(&meta)
        .unwrap()
        .read_exact(&mut sector)
        .unwrap();
    let vl5a_mask = FEATURE_INCOMPAT_KV_V3
        | FEATURE_INCOMPAT_NODE_SEQ_WATERMARK
        | FEATURE_INCOMPAT_KV_GUEST_SLOTS
        | FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE;
    let err = squeezefs::meta_backend::kv::superblock::SuperblockV3::decode_sector_with_known(
        &sector, vl5a_mask,
    )
    .expect_err("a VL5a known-mask must refuse a bit-4 superblock");
    assert!(
        format!("{err}").contains("bit 4"),
        "refusal must name the unknown bit: {err}"
    );
    squeezefs::meta_backend::kv::superblock::SuperblockV3::decode_sector_with_known(
        &sector,
        FEATURES_INCOMPAT_KNOWN,
    )
    .expect("this binary understands bit 4");
}

// ---------------------------------------------------------------------------
// Guest ino-namespace partition (the keyspace math)
// ---------------------------------------------------------------------------

#[test]
fn test_guest_ino_namespace_partition_disjoint_and_round_trips() {
    // Pin the namespace geometry (a change is an on-disk format change).
    assert_eq!(GUEST_NS_SHIFT, 40, "2^40 native locals per volume");
    assert_eq!(GUEST_NS_BASE, 1u64 << GUEST_NS_SHIFT);
    // Native locals never split as guests.
    for local in [1u64, 2, 3, GUEST_NS_BASE - 1] {
        assert_eq!(split_guest_local(local), None);
    }
    // Round trip + per-slot disjointness.
    let mut seen = std::collections::HashSet::new();
    for slot in [0u16, 1, 2, 63, 512, u16::MAX] {
        for raw in [1u64, 2, 4095, GUEST_NS_BASE - 1] {
            let eff = guest_local_ino(slot, raw);
            assert!(eff >= GUEST_NS_BASE, "guest inos live above the base");
            assert_eq!(
                split_guest_local(eff),
                Some((slot, raw)),
                "split(guest({slot},{raw}))"
            );
            assert!(seen.insert(eff), "namespaces must be disjoint");
        }
    }
}

// ---------------------------------------------------------------------------
// Extended stamp: native_slot + per-slot cursors, encode/decode, VL5a
// byte-identity when unextended
// ---------------------------------------------------------------------------

#[test]
fn test_extended_stamp_round_trip_and_v1_byte_identity() {
    let uuid = [7u8; 16];
    let v1 = MembershipStamp {
        set_uuid: uuid,
        set_epoch: 3,
        member_position: 1,
        member_count: 2,
        routing_width: 4,
        slots_hosted: vec![1, 3],
        native_slot: None,
        slot_cursors: Vec::new(),
    };
    let rec_of = |st: MembershipStamp| LedgerRecord {
        seq: 9,
        tree_roots: Vec::new(),
        journal_tail_seq: 1,
        next_ino: 5,
        alloc_bitmap_generation: 9,
        node_seq_watermark: 2,
        membership_stamp: Some(st),
    };
    // Unextended stamps keep the VL5a byte encoding exactly (KD-14's
    // untouched-sets law one level up: sets that never migrate stay
    // bit-identical).
    let img_v1 = rec_of(v1.clone()).encode_slot().expect("v1 encodes");
    let back = LedgerRecord::decode_slot(&img_v1).expect("v1 decodes");
    assert_eq!(back.membership_stamp.as_ref(), Some(&v1));

    let v2 = MembershipStamp {
        native_slot: Some(1),
        slot_cursors: vec![(3, 4711), (1, 99)],
        ..v1.clone()
    };
    let img_v2 = rec_of(v2.clone()).encode_slot().expect("v2 encodes");
    assert!(
        img_v2.len() == ROOT_LEDGER_SLOT_LEN as usize,
        "still one padded slot"
    );
    let back = LedgerRecord::decode_slot(&img_v2).expect("v2 decodes");
    assert_eq!(back.membership_stamp.as_ref(), Some(&v2));
    // The extension provably widens the payload past the VL5a equation
    // (decode-as-absent to VL5a binaries — the bit-4 rationale).
    let payload_len = |img: &[u8]| u32::from_le_bytes(img[4..8].try_into().unwrap()) as usize;
    assert!(payload_len(&img_v2) > payload_len(&img_v1));
}

// ---------------------------------------------------------------------------
// Tree-diff equivalence + guest keyspace isolation + non-participant
// byte-identity + ino stability (G-VL-4's core clause)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_slot_migration_equivalence_isolation_and_ino_stability() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
        make_file(dir.path(), "m2", VOL_LEN),
    ];
    // W = 2N: a flip must never leave a member hostless (every member
    // keeps its second hosted slot as the mint slot).
    format_stamped_set(&metas, 6).await;
    let paths = uris(&metas);

    let routed = open_routed_meta_set(&paths).await.expect("open");
    let made = populate(&routed, 6, 8).await;
    let digest_before = set_logical_digest(&routed).await.expect("digest");
    let slot1_before = slot_logical_digest(&routed, 1).await.expect("slot digest");

    shutdown_routed(&routed).await;
    drop(routed);

    // Migrate slot 1 (volume 1's native keyspace) to volume 0 — online
    // engine over a freshly opened routed set.
    let routed = open_routed_meta_set(&paths).await.expect("reopen");
    let report = migrate_slot(
        &routed,
        1,
        0,
        &MigrationOptions::default(),
        &MigrationTestHooks::default(),
    )
    .await
    .expect("migration succeeds");
    assert!(report.records_copied > 0, "engagement: records copied");

    // Equivalence + stability on the LIVE set (map swapped in RAM).
    assert_eq!(
        slot_logical_digest(&routed, 1).await.expect("slot digest"),
        slot1_before,
        "slot-1 logical tree diff must be ∅ post-cutover"
    );
    assert_eq!(
        set_logical_digest(&routed).await.expect("digest"),
        digest_before,
        "whole-set logical diff must be ∅"
    );
    verify_population(&routed, &made).await;

    // New mints in the migrated slot's HOST must not collide: create
    // files in a slot-1 directory (now hosted by volume 0 as a guest).
    let dino = routed.lookup(1, "dir0").await.expect("dir").ino;
    let extra = routed
        .create(dino, "post_migration", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create after migration");
    assert!(
        !made.iter().any(|(_, i)| *i == extra.ino),
        "per-slot cursors must prevent ino reuse after migration"
    );
    // The remount comparison baseline includes the post-migration create.
    let digest_with_extra = set_logical_digest(&routed).await.expect("digest");
    shutdown_routed(&routed).await;
    drop(routed);

    // Non-participation contract: volume 2 never grew bit 4, never
    // hosts a guest record, and its own slots' logical state is
    // untouched (ordinary mount activity — claims/checkpoints — moves
    // device bytes on EVERY volume, so the contract is representational,
    // not a device-image freeze).
    let squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb2) =
        classify_volume(&metas[2]).await.unwrap()
    else {
        panic!("v3");
    };
    assert_eq!(
        sb2.features_incompat & FEATURE_INCOMPAT_KV_SLOT_MIGRATION,
        0,
        "a non-participating volume must never grow incompat bit 4"
    );

    // Remount: discovery resolves the new map (highest-epoch-wins on the
    // participants), everything still resolves.
    let disc = discover_meta_set(&paths).await.expect("discovers");
    assert_eq!(
        disc.slot_to_volume[1], 0,
        "slot 1 must be hosted by volume 0 after the flip"
    );
    let routed = open_routed_meta_set(&paths).await.expect("post remount");
    verify_population(&routed, &made).await;
    assert_eq!(
        set_logical_digest(&routed).await.expect("digest"),
        digest_with_extra,
        "logical diff ∅ across remount"
    );
    shutdown_routed(&routed).await;
}

// ---------------------------------------------------------------------------
// Delta tee: concurrent commits during the copy are captured, values
// re-read at apply
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_delta_tee_captures_concurrent_writes_values_reread() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 4).await;
    let paths = uris(&metas);
    let routed = open_routed_meta_set(&paths).await.expect("open");
    let made = populate(&routed, 4, 4).await;

    // Hold the engine after its first bulk-copy pass so concurrent
    // mutations land AFTER the snapshot walk (deterministically inside
    // the tee window — no sleeps).
    let hold = Arc::new(PhaseHold::new(MigrationPhase::AfterBulkCopy));
    let hooks = MigrationTestHooks {
        hold: Some(hold.clone()),
    };
    let routed2 = routed.clone();
    let mig = tokio::spawn(async move {
        migrate_slot(&routed2, 1, 0, &MigrationOptions::default(), &hooks).await
    });
    hold.entered().await;

    // Mutations to the MIGRATING slot (slot 1) while the engine is
    // parked: a new file, an xattr rewrite (twice — values re-read
    // means the SECOND value must win), and an unlink.
    let mut dino = None;
    for d in 0..4 {
        let ino = routed.lookup(1, &format!("dir{d}")).await.expect("dir").ino;
        if route_ino_width(ino, 4).0 == 1 {
            dino = Some(ino);
            break;
        }
    }
    let dino = dino.expect("a slot-1 directory exists among dir0..dir3");
    let fresh = routed
        .create(dino, "teed_create", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create during copy")
        .ino;
    let victim = routed.lookup(dino, "f0").await.expect("victim").ino;
    routed
        .setxattr(victim, "user.tag", b"stale-value")
        .await
        .expect("first rewrite");
    routed
        .setxattr(victim, "user.tag", b"final-value")
        .await
        .expect("second rewrite");
    routed.unlink(dino, "f1").await.expect("unlink during copy");

    hold.release();
    let report = mig.await.unwrap().expect("migration succeeds");
    assert!(
        report.delta_keys > 0,
        "the tee must have captured the concurrent commits: {report:?}"
    );

    // Post-cutover state serves the TEED mutations from the new host.
    let got = routed
        .lookup(dino, "teed_create")
        .await
        .expect("teed create resolves");
    assert_eq!(got.ino, fresh);
    let tag = routed
        .getxattr(victim, "user.tag")
        .await
        .expect("getxattr")
        .expect("present");
    assert_eq!(
        tag, b"final-value",
        "values are re-read at apply — the LAST committed value wins"
    );
    assert!(
        routed.lookup(dino, "f1").await.is_err(),
        "the teed unlink must hold post-cutover"
    );
    // The untouched population is intact too (minus the teed unlink,
    // whichever slot-1 directory it hit).
    let unlinked_parent = made
        .iter()
        .find(|(_, i)| *i == dino)
        .map(|(n, _)| n.clone())
        .expect("the slot-1 dir is in the population");
    let survivors: Vec<(String, u64)> = made
        .iter()
        .filter(|(n, _)| {
            *n != format!("{unlinked_parent}/f1") && *n != format!("{unlinked_parent}/f0")
        })
        .cloned()
        .collect();
    verify_population(&routed, &survivors).await;
    shutdown_routed(&routed).await;
}

// ---------------------------------------------------------------------------
// Delta overflow: fresh-snapshot fallback; triple overflow aborts loud
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_delta_overflow_fresh_snapshot_fallback_and_triple_abort() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 4).await;
    let paths = uris(&metas);
    let routed = open_routed_meta_set(&paths).await.expect("open");
    populate(&routed, 2, 4).await;
    let digest_before = set_logical_digest(&routed).await.expect("digest");

    // (a) Fallback: a tiny side log overflows under held-copy mutations;
    // the engine flips to a fresh snapshot pass and still converges.
    // (`once`: only the FIRST snapshot round is held — the fallback
    // round must run free or the test would hang the engine.)
    let hold = Arc::new(PhaseHold::once(MigrationPhase::AfterBulkCopy));
    let hooks = MigrationTestHooks {
        hold: Some(hold.clone()),
    };
    let opts_small = MigrationOptions {
        delta_log_cap: 4,
        ..MigrationOptions::default()
    };
    let routed2 = routed.clone();
    let mig = tokio::spawn(async move { migrate_slot(&routed2, 1, 0, &opts_small, &hooks).await });
    hold.entered().await;
    let mut dino = None;
    for d in 0..2 {
        let ino = routed.lookup(1, &format!("dir{d}")).await.expect("dir").ino;
        if route_ino_width(ino, 4).0 == 1 {
            dino = Some(ino);
            break;
        }
    }
    // populate() striped only 2 dirs — if neither landed on slot 1,
    // mint fresh dirs until one does (health round-robin alternates).
    let dino = match dino {
        Some(i) => i,
        None => {
            // The engine is parked at the hold — creates still flow (the
            // gate is open during snapshot passes).
            let mut found = None;
            for i in 0..8 {
                let ino = routed
                    .create(1, &format!("ovdir{i}"), libc::S_IFDIR | 0o755, 0, 0)
                    .await
                    .expect("mkdir")
                    .ino;
                if route_ino_width(ino, 4).0 == 1 {
                    found = Some(ino);
                    break;
                }
            }
            found.expect("a slot-1 dir within 8 mkdirs")
        }
    };
    for i in 0..32 {
        routed
            .create(dino, &format!("ov{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .expect("overflow filler");
    }
    hold.release();
    let report = mig
        .await
        .unwrap()
        .expect("overflow must FALL BACK, not fail");
    assert!(
        report.overflows >= 1,
        "the tiny log must have overflowed: {report:?}"
    );
    for i in 0..32 {
        routed
            .lookup(dino, &format!("ov{i}"))
            .await
            .unwrap_or_else(|e| panic!("ov{i} lost across the fallback: {e}"));
    }
    assert_ne!(
        set_logical_digest(&routed).await.expect("digest"),
        digest_before,
        "sanity: the fillers changed the logical state"
    );

    // (b) Triple consecutive overflow aborts LOUD: a FRESH set (part (a)
    // already moved slot 1 on the first one — no new dirs can mint there
    // any more), hold every snapshot round, and mutate the migrating
    // keyspace once per round so each round's zero-cap log overflows.
    shutdown_routed(&routed).await;
    drop(routed);
    let metas_b = vec![
        make_file(dir.path(), "b0", VOL_LEN),
        make_file(dir.path(), "b1", VOL_LEN),
    ];
    format_stamped_set(&metas_b, 4).await;
    let paths_b = uris(&metas_b);
    let routed = open_routed_meta_set(&paths_b).await.expect("open b");
    let slot1_dir = {
        let mut found = None;
        for i in 0..8 {
            let ino = routed
                .create(1, &format!("abortdir{i}"), libc::S_IFDIR | 0o755, 0, 0)
                .await
                .expect("mkdir")
                .ino;
            if route_ino_width(ino, 4).0 == 1 {
                found = Some(ino);
                break;
            }
        }
        found.expect("a slot-1 directory materializes within 8 mkdirs")
    };
    let hold = Arc::new(PhaseHold::new(MigrationPhase::AfterBulkCopy));
    let hold_for_writer = hold.clone();
    let hooks = MigrationTestHooks {
        hold: Some(hold.clone()),
    };
    let opts_zero = MigrationOptions {
        delta_log_cap: 0, // every concurrent commit overflows the round
        ..MigrationOptions::default()
    };
    let routed2 = routed.clone();
    let routed_writer = routed.clone();
    let writer = tokio::spawn(async move {
        for i in 0..16u64 {
            // Park until the engine holds a snapshot round, mutate the
            // migrating keyspace once, release the round.
            hold_for_writer.entered().await;
            let _ = routed_writer
                .create(slot1_dir, &format!("abort{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await;
            hold_for_writer.release();
        }
    });
    let err = migrate_slot(&routed2, 1, 0, &opts_zero, &hooks)
        .await
        .expect_err("three consecutive overflows must abort loud");
    writer.abort();
    let msg = format!("{err}");
    assert!(
        msg.contains("overflow"),
        "abort must name the overflow rule: {msg}"
    );
    // The aborted migration leaves the OLD map serving (nothing flipped),
    // no fail-stop escalation, and the tree intact.
    assert!(
        routed.disabled_volumes.is_empty(),
        "an aborted migration must never escalate to disabled_volumes"
    );
    routed
        .lookup(1, "abortdir0")
        .await
        .expect("the old map still serves after the abort");
    shutdown_routed(&routed).await;
}

// ---------------------------------------------------------------------------
// §5.5.2a: the cutover gate — parked ops hold ZERO 4a guards, no
// disabled_volumes escalation, the cross-slot rename's legs are captured
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cutover_gate_parks_cross_slot_rename_with_zero_guards() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 4).await;
    let paths = uris(&metas);
    let routed = open_routed_meta_set(&paths).await.expect("open");
    populate(&routed, 4, 2).await;

    // Find a directory hosted on volume 1 (the migrating slot 1) and one
    // on volume 0 — the cross-slot rename spans them.
    let mut src_dir = None;
    let mut dst_dir = None;
    for d in 0..4 {
        let ino = routed.lookup(1, &format!("dir{d}")).await.expect("dir").ino;
        let (slot, _) = route_ino_width(ino, 4);
        if slot == 1 && src_dir.is_none() {
            src_dir = Some(ino);
        }
        if slot == 0 && dst_dir.is_none() {
            dst_dir = Some(ino);
        }
    }
    let (src_dir, dst_dir) = (
        src_dir.expect("a slot-1 directory exists"),
        dst_dir.expect("a slot-0 directory exists"),
    );

    let parked_before = squeezefs::fuse_client::METRICS
        .meta_slot_gate_parked_commits
        .load(std::sync::atomic::Ordering::Relaxed);

    // Hold the engine INSIDE the closed-gate window (deterministic: the
    // hook parks the cutover task after the gate closes, before the
    // final delta).
    let hold = Arc::new(PhaseHold::new(MigrationPhase::GateClosed));
    let hooks = MigrationTestHooks {
        hold: Some(hold.clone()),
    };
    // A wide-open cutover deadline so the held window cannot abort under
    // us (the abort-and-retry law is exercised separately below).
    let mig_opts = MigrationOptions {
        cutover_deadline: std::time::Duration::from_secs(60),
        ..MigrationOptions::default()
    };
    let routed2 = routed.clone();
    let mig = tokio::spawn(async move { migrate_slot(&routed2, 1, 0, &mig_opts, &hooks).await });
    hold.entered().await;

    // The rename spanning the migrating slot: issued while the gate is
    // closed — it must PARK at the gate.
    let routed3 = routed.clone();
    let rename =
        tokio::spawn(async move { routed3.rename(src_dir, "f0", dst_dir, "moved_in", 0).await });
    // Deterministic park proof: the gate-parked counter moves...
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let parked_now = squeezefs::fuse_client::METRICS
            .meta_slot_gate_parked_commits
            .load(std::sync::atomic::Ordering::Relaxed);
        if parked_now > parked_before {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the cross-slot rename never parked at the gate"
        );
        tokio::task::yield_now().await;
    }
    assert!(!rename.is_finished(), "the rename must still be parked");

    // ...and the parked op holds ZERO 4a guards: the same parents'
    // exclusive I+D stripes are immediately acquirable (a held guard
    // would deadlock this probe).
    for (ino, name) in [(src_dir, "f0"), (dst_dir, "moved_in")] {
        let (v_idx, local) = routed.route_ino(ino);
        let probe = routed
            .volume_dlm(v_idx)
            .lock_many(
                &[(local, squeezefs::meta_backend::dlm::LockMode::Exclusive)],
                &[(
                    local,
                    name,
                    squeezefs::meta_backend::dlm::LockMode::Exclusive,
                )],
            )
            .await;
        drop(probe);
    }

    // No fail-stop escalation: gate parks are planned, never
    // disabled_volumes (§5.5.2a's carve-out).
    assert!(
        routed.disabled_volumes.is_empty(),
        "gate parks must NEVER escalate to disabled_volumes"
    );

    // Release the window: the migration completes, the rename lands, and
    // its slot-1 leg was captured by the delta (the moved dentry serves
    // from the new host).
    hold.release();
    mig.await.unwrap().expect("migration completes");
    rename.await.unwrap().expect("the parked rename completes");
    let got = routed
        .lookup(dst_dir, "moved_in")
        .await
        .expect("the rename's destination leg resolves post-cutover");
    assert!(
        routed.lookup(src_dir, "f0").await.is_err(),
        "the rename's source leg resolves post-cutover (dentry gone)"
    );
    let _ = got;
    shutdown_routed(&routed).await;
}

/// The §5.5.2a escalation carve-out's OTHER half: a cutover window that
/// cannot drain within its deadline aborts-and-retries (gate reopened,
/// delta rounds resume) — it never wedges parked ops and never trips
/// fail-stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cutover_deadline_aborts_and_retries_never_escalates() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 4).await;
    let paths = uris(&metas);
    let routed = open_routed_meta_set(&paths).await.expect("open");
    populate(&routed, 2, 2).await;

    // Hold the closed-gate window LONGER than the (tiny) deadline: the
    // engine must abort the window, reopen the gate, and retry — and
    // still converge once the hold stops re-arming.
    let hold = Arc::new(PhaseHold::once(MigrationPhase::GateClosed));
    let hooks = MigrationTestHooks {
        hold: Some(hold.clone()),
    };
    let mig_opts = MigrationOptions {
        cutover_deadline: std::time::Duration::from_millis(50),
        ..MigrationOptions::default()
    };
    let routed2 = routed.clone();
    let mig = tokio::spawn(async move { migrate_slot(&routed2, 1, 0, &mig_opts, &hooks).await });
    hold.entered().await;
    // Park PAST the deadline (the hold keeps the window open while the
    // deadline clock runs out).
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    hold.release();
    let report = mig.await.unwrap().expect("abort-and-retry must converge");
    assert!(
        report.cutover_retries >= 1,
        "the deadline must have forced at least one abort-and-retry: {report:?}"
    );
    assert!(
        routed.disabled_volumes.is_empty(),
        "deadline aborts must never escalate to disabled_volumes"
    );
    shutdown_routed(&routed).await;
}

// ---------------------------------------------------------------------------
// §5.5.2b crash windows: kill after writes 0/1/2/3 + torn slot; observed
// states match the table; re-run converges
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_flip_crash_windows_resolve_and_rerun_converges() {
    for window in [0u8, 1, 2, 3] {
        for round in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let metas = vec![
                make_file(dir.path(), "m0", VOL_LEN),
                make_file(dir.path(), "m1", VOL_LEN),
            ];
            format_stamped_set(&metas, 4).await;
            let paths = uris(&metas);
            let routed = open_routed_meta_set(&paths).await.expect("open");
            let made = populate(&routed, 2, 3).await;
            let digest_before = set_logical_digest(&routed).await.expect("digest");

            let crash_opts = MigrationOptions {
                crash_after_write: Some(window),
                ..MigrationOptions::default()
            };
            let err = migrate_slot(&routed, 1, 0, &crash_opts, &MigrationTestHooks::default())
                .await
                .expect_err("the crash seam must abort the flip");
            assert!(
                format!("{err}").contains("crash injection"),
                "window {window} round {round}: {err}"
            );
            shutdown_routed(&routed).await;
            drop(routed);

            // The observed state lands in the table: window 0 ⇒ old map;
            // windows 1..3 ⇒ B hosts the slot (per-slot highest-epoch-wins
            // resolves the dual claim of window 1).
            let disc = discover_meta_set(&paths)
                .await
                .expect("every crash prefix must stay mountable (window {window})");
            let expect_host = if window == 0 { 1 } else { 0 };
            assert_eq!(
                disc.slot_to_volume[1], expect_host,
                "window {window}: observed state must match the §5.5.2b table"
            );

            // Re-run converges: the migration is idempotent.
            let routed = open_routed_meta_set(&paths).await.expect("reopen");
            migrate_slot(
                &routed,
                1,
                0,
                &MigrationOptions::default(),
                &MigrationTestHooks::default(),
            )
            .await
            .expect("re-run must converge");
            assert_eq!(
                set_logical_digest(&routed).await.expect("digest"),
                digest_before,
                "window {window} round {round}: logical diff ∅ after re-run"
            );
            verify_population(&routed, &made).await;
            shutdown_routed(&routed).await;
        }
    }
}

/// Mid-write torn slot: corrupt the TARGET's newest (claim-carrying)
/// ledger slot — A/B fallback surfaces the pre-claim record; the old map
/// serves; re-run converges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_flip_torn_claim_slot_falls_back_and_rerun_converges() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 4).await;
    let paths = uris(&metas);
    let routed = open_routed_meta_set(&paths).await.expect("open");
    let made = populate(&routed, 2, 3).await;
    let digest_before = set_logical_digest(&routed).await.expect("digest");

    // Crash right after write 1 (B's claim) — then TEAR that ledger slot.
    let crash_opts = MigrationOptions {
        crash_after_write: Some(1),
        ..MigrationOptions::default()
    };
    migrate_slot(&routed, 1, 0, &crash_opts, &MigrationTestHooks::default())
        .await
        .expect_err("crash seam");
    // A kill-9 analog: DROP without clean shutdown (no final checkpoint
    // may re-write the claim after the tear below; the checkpoint task
    // reaps on its next tick — the documented sentinel discipline).
    drop(routed);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Tear EVERY claim-carrying ledger slot on the TARGET (volume 0):
    // the mid-write-torn-claim state — A/B selection must fall back to
    // the newest intact (pre-claim) record.
    let squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) =
        classify_volume(&metas[0]).await.unwrap()
    else {
        panic!("v3");
    };
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&metas[0])
        .unwrap();
    for slot_idx in 0..32u64 {
        let mut img = vec![0u8; ROOT_LEDGER_SLOT_LEN as usize];
        f.seek(SeekFrom::Start(
            sb.root_ledger.start + slot_idx * ROOT_LEDGER_SLOT_LEN,
        ))
        .unwrap();
        f.read_exact(&mut img).unwrap();
        let Ok(rec) = LedgerRecord::decode_slot(&img) else {
            continue;
        };
        if rec
            .membership_stamp
            .as_ref()
            .is_some_and(|st| st.slots_hosted.contains(&1))
        {
            f.seek(SeekFrom::Start(
                sb.root_ledger.start + slot_idx * ROOT_LEDGER_SLOT_LEN + 48,
            ))
            .unwrap();
            f.write_all(&[0xFF; 32]).unwrap();
        }
    }
    f.sync_all().unwrap();

    // Torn claim ⇒ predecessor record ⇒ OLD claim state ⇒ old map.
    let disc = discover_meta_set(&paths)
        .await
        .expect("torn slot must fall back, not refuse");
    assert_eq!(
        disc.slot_to_volume[1], 1,
        "a torn claim slot loses to its intact predecessor — the old map serves"
    );

    // Re-run converges to the new map with an intact tree.
    let routed = open_routed_meta_set(&paths).await.expect("reopen");
    migrate_slot(
        &routed,
        1,
        0,
        &MigrationOptions::default(),
        &MigrationTestHooks::default(),
    )
    .await
    .expect("re-run converges");
    assert_eq!(
        set_logical_digest(&routed).await.expect("digest"),
        digest_before
    );
    verify_population(&routed, &made).await;
    shutdown_routed(&routed).await;
    let disc = discover_meta_set(&paths).await.expect("discovers");
    assert_eq!(disc.slot_to_volume[1], 0, "converged to the new map");
}

// ---------------------------------------------------------------------------
// KD-8: the staging drain barrier + generation restamp
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_staging_drain_barrier_refuses_custody_and_restamps() {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    // A dir bound to the OLD generation carrying NO write custody:
    // the barrier restamps it in place (read cache preserved — the
    // discard law becomes a provable no-op).
    squeezefs::cache::write_staging_generation_marker(&staging, "old-gen")
        .await
        .expect("stamp old generation");
    squeezefs::config_ops::staging_drain_barrier(
        std::slice::from_ref(&staging),
        "old-gen",
        "new-gen",
    )
    .await
    .expect("custody-free dir restamps");
    assert_eq!(
        squeezefs::cache::read_staging_generation_marker(&staging)
            .await
            .expect("marker readable"),
        Some("new-gen".to_string()),
        "the barrier must restamp the new generation"
    );

    // A dir carrying staged WRITE CUSTODY refuses loud, naming the unit
    // (R7: per-unit diagnostics, abortable, never a drop) — and leaves
    // the marker untouched.
    let dirty = dir.path().join("dirty");
    std::fs::create_dir_all(&dirty).unwrap();
    squeezefs::cache::write_staging_generation_marker(&dirty, "new-gen")
        .await
        .expect("stamp");
    squeezefs::cache::seed_staged_custody_for_test(&dirty, "active_block:42:7")
        .await
        .expect("seed a staged custody record");
    let err = squeezefs::config_ops::staging_drain_barrier(
        std::slice::from_ref(&dirty),
        "new-gen",
        "next-gen",
    )
    .await
    .expect_err("staged custody must refuse the barrier");
    let msg = format!("{err}");
    assert!(
        msg.contains("active_block:42:7"),
        "the refusal must carry per-unit diagnostics: {msg}"
    );
    assert_eq!(
        squeezefs::cache::read_staging_generation_marker(&dirty)
            .await
            .unwrap(),
        Some("new-gen".to_string()),
        "an aborted barrier must leave the old binding untouched (nothing flipped)"
    );
}

// ---------------------------------------------------------------------------
// add-meta / remove-meta end to end (offline coordinator posture) + ino
// stability + tombstone refusal + highest-epoch-wins bootstrap
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_add_meta_and_remove_meta_end_to_end_ino_stable() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 8).await;
    let paths = uris(&metas);

    // Populate, remember, close.
    let routed = open_routed_meta_set(&paths).await.expect("open");
    let made = populate(&routed, 4, 6).await;
    let digest_before = set_logical_digest(&routed).await.expect("digest");
    shutdown_routed(&routed).await;
    drop(routed);
    let gen_before = volume_set_generation(&paths).await.expect("generation");

    // add-meta a third volume, taking two slots.
    let m2 = make_file(dir.path(), "m2", VOL_LEN);
    let taken = squeezefs::config_ops::add_meta_volume(
        &paths,
        &m2.display().to_string(),
        &squeezefs::config_ops::TakeSlots::Count(2),
    )
    .await
    .expect("add-meta succeeds");
    assert_eq!(taken.len(), 2, "two slots taken: {taken:?}");

    // The new 3-member set mounts; the generation CHANGED (membership
    // change); every ino is stable; logical diff ∅.
    let mut new_paths = paths.clone();
    new_paths.push(m2.display().to_string());
    let gen_after = volume_set_generation(&new_paths).await.expect("generation");
    assert_ne!(gen_before, gen_after, "membership changes the generation");
    let disc = discover_meta_set(&new_paths).await.expect("discovers");
    for s in &taken {
        assert_eq!(
            disc.slot_to_volume[*s as usize], 2,
            "taken slot {s} hosted by the new member"
        );
    }
    let routed = open_routed_meta_set(&new_paths)
        .await
        .expect("open new set");
    verify_population(&routed, &made).await;
    assert_eq!(
        set_logical_digest(&routed).await.expect("digest"),
        digest_before,
        "logical diff ∅ across add-meta"
    );
    // The old 2-member URI now refuses loud (stamps declare 3 members).
    shutdown_routed(&routed).await;
    drop(routed);
    let err = discover_meta_set(&paths)
        .await
        .expect_err("the shrunken URI must refuse after add-meta");
    assert!(format!("{err}").contains('3'), "{err}");

    // remove-meta the ORIGINAL volume 1: its hosted slots migrate to the
    // survivors, the victim tombstones, the survivor set mounts with
    // every ino stable.
    let survivor_paths = vec![new_paths[0].clone(), new_paths[2].clone()];
    squeezefs::config_ops::remove_meta_volume(&new_paths, &new_paths[1])
        .await
        .expect("remove-meta succeeds");
    let routed = open_routed_meta_set(&survivor_paths)
        .await
        .expect("survivor set mounts");
    verify_population(&routed, &made).await;
    assert_eq!(
        set_logical_digest(&routed).await.expect("digest"),
        digest_before,
        "logical diff ∅ across remove-meta"
    );
    // New creates still mint non-colliding inos.
    let extra = routed
        .create(1, "post_remove", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create after remove-meta");
    assert!(!made.iter().any(|(_, i)| *i == extra.ino));
    shutdown_routed(&routed).await;
    drop(routed);

    // Listing the retired victim refuses loud (tombstone).
    let err = discover_meta_set(&new_paths)
        .await
        .expect_err("a retired member in the URI must refuse loud");
    let msg = format!("{err}");
    assert!(
        msg.contains("retired") || msg.contains("tombstone") || msg.contains("member"),
        "the refusal must name the retirement: {msg}"
    );
}

/// remove-meta preflight: survivors without the §5.2 meta-side capacity
/// refuse with the honest numbers before anything is touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_remove_meta_capacity_preflight_refuses_honestly() {
    let dir = tempfile::tempdir().unwrap();
    // A tiny survivor: cannot absorb the victim's extents + headroom.
    let m0 = make_file(dir.path(), "m0", 32 * 1024 * 1024);
    let m1 = make_file(dir.path(), "m1", VOL_LEN);
    let plan = plan_meta_slot_set(2, 2).expect("plan");
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &m0,
        32 * 1024 * 1024,
        &opts(),
        plan.stamps[0].clone(),
    )
    .await
    .unwrap();
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &m1,
        VOL_LEN,
        &opts(),
        plan.stamps[1].clone(),
    )
    .await
    .unwrap();
    let paths = vec![m0.display().to_string(), m1.display().to_string()];

    // Fill the victim (m1) with enough records that its used extents
    // cannot fit the midget survivor's free extents minus headroom.
    let routed = open_routed_meta_set(&paths).await.expect("open");
    for d in 0..8 {
        let dino = routed
            .create(1, &format!("bulk{d}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .expect("mkdir")
            .ino;
        for i in 0..256 {
            let ino = routed
                .create(dino, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .expect("create")
                .ino;
            routed
                .setxattr(ino, "user.pad", &vec![0xAB; 8192])
                .await
                .expect("xattr pad");
        }
    }
    shutdown_routed(&routed).await;
    drop(routed);

    let err = squeezefs::config_ops::remove_meta_volume(&paths, &paths[1])
        .await
        .expect_err("the midget survivor must refuse the §5.2 meta preflight");
    let msg = format!("{err}");
    assert!(
        msg.contains("extent") || msg.contains("preflight"),
        "the refusal must carry the §5.2 meta terms: {msg}"
    );
    // Nothing was touched: the original set still mounts coherently.
    discover_meta_set(&paths).await.expect("set untouched");
}

// ---------------------------------------------------------------------------
// VL8 item 8 — §5.5.2b add-meta crash windows: the documented SAME-ARGUMENTS
// re-run must CONVERGE, not refuse. Found by the VL7 rig (leg 11 loop 3,
// kill at ~0.7 s): a coordinator killed after stamping the (n+1)-member set
// left a re-run refusing with "stamps declare 3 members but the URI lists 2".
// ---------------------------------------------------------------------------

/// The stamped-ahead window (crash after writes 2..n, before teardown +
/// staging finalize + config mirror): every stamp already declares 3
/// members. The re-run with the SAME arguments (old 2-member URI + the new
/// device) must detect the stamped-ahead state and converge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_add_meta_rerun_converges_after_survivor_stamps_crash() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 8).await;
    let paths = uris(&metas);

    let routed = open_routed_meta_set(&paths).await.expect("open");
    let made = populate(&routed, 3, 5).await;
    let digest_before = set_logical_digest(&routed).await.expect("digest");
    shutdown_routed(&routed).await;
    drop(routed);

    // Crash the coordinator AFTER every member is stamped @count 3.
    let m2 = make_file(dir.path(), "m2", VOL_LEN);
    let err = squeezefs::config_ops::add_meta_volume_with(
        &paths,
        &m2.display().to_string(),
        &squeezefs::config_ops::TakeSlots::Count(2),
        &squeezefs::config_ops::AddMetaHooks {
            crash_after: Some(squeezefs::config_ops::AddMetaCrash::SurvivorStamps),
        },
    )
    .await
    .expect_err("the crash seam must abort the coordinator");
    assert!(
        format!("{err}").contains("crash injection"),
        "seam abort, not a real failure: {err}"
    );

    // The documented recovery: re-run with the SAME arguments.
    let taken = squeezefs::config_ops::add_meta_volume(
        &paths,
        &m2.display().to_string(),
        &squeezefs::config_ops::TakeSlots::Count(2),
    )
    .await
    .expect("the same-arguments re-run must converge (§5.5.2b)");
    assert_eq!(taken.len(), 2, "resumed claim slots: {taken:?}");

    // The converged 3-member set mounts with every ino stable, diff ∅.
    let mut new_paths = paths.clone();
    new_paths.push(m2.display().to_string());
    let routed = open_routed_meta_set(&new_paths).await.expect("open new set");
    verify_population(&routed, &made).await;
    assert_eq!(
        set_logical_digest(&routed).await.expect("digest"),
        digest_before,
        "logical diff ∅ across the crashed-then-resumed add-meta"
    );
    shutdown_routed(&routed).await;
    drop(routed);
}

/// The claim window (crash after write 1 — the new member's durable claim —
/// before any survivor re-stamp): survivors still declare the old count.
/// The same-arguments re-run resumes the claim and converges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_add_meta_rerun_converges_after_new_member_claim_crash() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 8).await;
    let paths = uris(&metas);

    let routed = open_routed_meta_set(&paths).await.expect("open");
    let made = populate(&routed, 2, 4).await;
    let digest_before = set_logical_digest(&routed).await.expect("digest");
    shutdown_routed(&routed).await;
    drop(routed);

    let m2 = make_file(dir.path(), "m2", VOL_LEN);
    let err = squeezefs::config_ops::add_meta_volume_with(
        &paths,
        &m2.display().to_string(),
        &squeezefs::config_ops::TakeSlots::Count(2),
        &squeezefs::config_ops::AddMetaHooks {
            crash_after: Some(squeezefs::config_ops::AddMetaCrash::NewMemberClaim),
        },
    )
    .await
    .expect_err("the crash seam must abort the coordinator");
    assert!(format!("{err}").contains("crash injection"), "{err}");

    let taken = squeezefs::config_ops::add_meta_volume(
        &paths,
        &m2.display().to_string(),
        &squeezefs::config_ops::TakeSlots::Count(2),
    )
    .await
    .expect("the same-arguments re-run must converge (§5.5.2b)");
    assert_eq!(taken.len(), 2, "resumed claim slots: {taken:?}");

    let mut new_paths = paths.clone();
    new_paths.push(m2.display().to_string());
    let routed = open_routed_meta_set(&new_paths).await.expect("open new set");
    verify_population(&routed, &made).await;
    assert_eq!(
        set_logical_digest(&routed).await.expect("digest"),
        digest_before,
        "logical diff ∅ across the crashed-then-resumed add-meta"
    );
    shutdown_routed(&routed).await;
    drop(routed);
}
