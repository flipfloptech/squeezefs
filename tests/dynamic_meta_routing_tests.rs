//! Dynamic meta routing — red-first contract battery
//! (docs/design-dynamic-meta-routing.md §7; user ruling 2026-08-02: the
//! frozen, user-chosen routing width must be dynamic).
//!
//! Contracts pinned here:
//! - **Derived width, no knob**: `plan_meta_slot_set(v)` derives
//!   `W = DERIVED_ROUTING_WIDTH = 2^16` (the slot-id namespace) for every
//!   volume count; one stride run per member; the fresh stamp fits the
//!   4096-B ledger slot with headroom.
//! - **`KV_DYNAMIC_ROUTING` = incompat bit 6** (forward-only, KD-14
//!   pattern): non-intersection with the pre-campaign known mask pinned;
//!   bit-6-ABSENT v3 volumes refuse loud naming the reformat remedy (the
//!   NODE_SEQ_WATERMARK presence-required precedent) — that one refusal
//!   covers both legacy identity sets and `--meta-slots`-era sets.
//! - **`SlotSet` stride runs**: coalescing, membership, mutation,
//!   validation refusals, and the run/cursor encoding-budget caps that
//!   replace the retired 64-hosted-slot cap (`encode_slot` boundary law:
//!   cap encodes, cap+1 refuses loud).
//! - **Mint spread**: minting on a volume rotates across
//!   `min(MINT_SPREAD, hosted)` slots — what makes the derived width REAL
//!   granularity (§2.3's cosmetic-W trap); cursors persist across remount
//!   with no global-ino collision.
//! - **Routing equivalence + ino stability at the derived W**: global-ino
//!   round-trip ∀ slots; created inos byte-stable across a real slot
//!   migration and across remounts.
//! - **Bootstrap order-independence at the derived W** (§5.5.1a re-run).
//! - **`format --meta-slots` dies loud naming its successor** (CLI).

use proptest::prelude::*;
use squeezefs::meta_backend::kv::checkpoint::{
    LedgerRecord, MembershipStamp, TreeRoot, STAMP_MAX_CURSORS, STAMP_MAX_RUNS,
};
use squeezefs::meta_backend::kv::slot_set::{SlotRun, SlotSet};
use squeezefs::meta_backend::kv::superblock::{
    FEATURES_INCOMPAT_KNOWN, FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING, FEATURE_INCOMPAT_KV_GUEST_SLOTS,
    FEATURE_INCOMPAT_KV_LAYOUT_DELTAS, FEATURE_INCOMPAT_KV_SLOT_MIGRATION, FEATURE_INCOMPAT_KV_V3,
    FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE, FEATURE_INCOMPAT_NODE_SEQ_WATERMARK,
};
use squeezefs::meta_backend::{
    discover_meta_set, make_global_ino_width, open_routed_meta_set, plan_meta_slot_set,
    route_ino_width, volume_set_generation, Metadata, DERIVED_ROUTING_WIDTH, MINT_SPREAD,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const VOL_LEN: u64 = 256 * 1024 * 1024;
const W: u32 = DERIVED_ROUTING_WIDTH;

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

/// Format a whole derived-width set the way `squeezefs format` does now:
/// one plan (derived W, one stride run per member), per-volume stamped
/// images.
async fn format_set(metas: &[PathBuf]) -> squeezefs::meta_backend::MetaSlotPlan {
    let plan = plan_meta_slot_set(metas.len()).expect("derived plan always admits");
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
    plan
}

fn uris(metas: &[PathBuf]) -> Vec<String> {
    metas.iter().map(|p| p.display().to_string()).collect()
}

async fn shutdown_routed(routed: &Arc<squeezefs::meta_backend::RoutedMetaBackend>) {
    for vol in &routed.volumes {
        vol.shutdown().await.unwrap();
    }
}

fn roots(n: usize) -> Vec<TreeRoot> {
    (0..n)
        .map(|i| TreeRoot {
            tree_id: (i % 250) as u8,
            node_addr: 0x1000 + i as u64 * 0x100,
            node_seq: 7 + i as u64,
        })
        .collect()
}

fn rec(stamp: Option<MembershipStamp>) -> LedgerRecord {
    LedgerRecord {
        seq: 3,
        tree_roots: roots(5),
        journal_tail_seq: 11,
        next_ino: 42,
        alloc_bitmap_generation: 3,
        node_seq_watermark: 99,
        membership_stamp: stamp,
    }
}

fn fresh_stamp(pos: u16, count: u16) -> MembershipStamp {
    let plan = plan_meta_slot_set(usize::from(count)).unwrap();
    let mut st = plan.stamps[usize::from(pos)].clone();
    st.set_uuid = [7u8; 16];
    st
}

// ---------------------------------------------------------------------------
// KD-14: KV_DYNAMIC_ROUTING = bit 6, forward-only refusal matrix
// ---------------------------------------------------------------------------

#[test]
fn test_dynamic_routing_bit_is_bit6_and_does_not_intersect_pre_campaign_mask() {
    assert_eq!(FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING, 1 << 6, "bit 6");
    // The pre-campaign binary's FEATURES_INCOMPAT_KNOWN was bits 0..=5.
    let pre_campaign = FEATURE_INCOMPAT_KV_V3
        | FEATURE_INCOMPAT_NODE_SEQ_WATERMARK
        | FEATURE_INCOMPAT_KV_GUEST_SLOTS
        | FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE
        | FEATURE_INCOMPAT_KV_SLOT_MIGRATION
        | FEATURE_INCOMPAT_KV_LAYOUT_DELTAS;
    assert_eq!(
        FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING & pre_campaign,
        0,
        "bit 6 must not intersect the pre-campaign known mask (bits 0..=5)"
    );
    assert_ne!(
        FEATURES_INCOMPAT_KNOWN & FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING,
        0,
        "this binary must understand bit 6"
    );
}

/// Fresh formats carry bit 6 (plus the stamp bits) on EVERY member —
/// including the plain single-volume `format_v3` path every test sandbox
/// uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_fresh_formats_carry_dynamic_routing_bit() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3(&meta, VOL_LEN, &opts())
        .await
        .expect("plain format");
    match squeezefs::meta_backend::kv::superblock::classify_volume(&meta)
        .await
        .expect("classify")
    {
        squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) => {
            for (bit, name) in [
                (FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING, "bit 6"),
                (FEATURE_INCOMPAT_KV_GUEST_SLOTS, "bit 2"),
                (FEATURE_INCOMPAT_KV_SLOT_MIGRATION, "bit 4"),
            ] {
                assert_ne!(
                    sb.features_incompat & bit,
                    0,
                    "fresh format must carry {name}"
                );
            }
        }
        other => panic!("fresh format classifies V3, got {other:?}"),
    }
}

/// A v3 volume WITHOUT bit 6 (a frozen-width-era format) refuses loud
/// with reformat guidance — the NODE_SEQ_WATERMARK presence-required
/// precedent. Crafted by clearing the bit on a real fresh superblock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_bit6_absent_v3_volume_refuses_loud_with_reformat_guidance() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3(&meta, VOL_LEN, &opts())
        .await
        .expect("plain format");
    let mut sb = match squeezefs::meta_backend::kv::superblock::classify_volume(&meta)
        .await
        .expect("classify")
    {
        squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) => sb,
        other => panic!("fresh format classifies V3, got {other:?}"),
    };
    sb.features_incompat &= !(FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING
        | FEATURE_INCOMPAT_KV_GUEST_SLOTS
        | FEATURE_INCOMPAT_KV_SLOT_MIGRATION);
    squeezefs::meta_backend::kv::superblock::write_superblock_v3(&meta, &sb)
        .await
        .expect("write back the frozen-width-shaped superblock");

    let err = squeezefs::meta_backend::open_volume_for_mount(&meta.display().to_string())
        .await
        .expect_err("bit-6-absent v3 volume must refuse the mount");
    let msg = format!("{err}");
    assert!(
        msg.contains("frozen routing width") || msg.contains("dynamic"),
        "refusal names the frozen-width era: {msg}"
    );
    assert!(
        msg.contains("reformat") || msg.contains("squeezefs format"),
        "refusal names the remedy: {msg}"
    );

    // Discovery (the mount bootstrap's first touch) refuses identically.
    let err2 = discover_meta_set(&[meta.display().to_string()])
        .await
        .expect_err("discovery refuses too");
    assert!(
        format!("{err2}").contains("reformat") || format!("{err2}").contains("squeezefs format"),
        "discovery refusal names the remedy: {err2}"
    );
}

// ---------------------------------------------------------------------------
// Derived width: no knob, one run per member, budget headroom
// ---------------------------------------------------------------------------

#[test]
fn test_derived_width_is_the_slot_id_namespace() {
    assert_eq!(
        DERIVED_ROUTING_WIDTH,
        u32::from(u16::MAX) + 1,
        "W = the full u16 slot-id namespace (2^16)"
    );
}

#[test]
fn test_plan_derives_width_one_run_per_member_and_fits_budget() {
    for v in [1usize, 2, 3, 8] {
        let plan = plan_meta_slot_set(v).expect("derived plan admits any volume count");
        assert_eq!(plan.routing_width, W, "V={v}: width derived, not chosen");
        assert_eq!(plan.stamps.len(), v);
        let mut total = 0u64;
        for (pos, st) in plan.stamps.iter().enumerate() {
            assert_eq!(st.routing_width, W);
            assert_eq!(usize::from(st.member_position), pos);
            assert_eq!(usize::from(st.member_count), v);
            assert_eq!(
                st.slots_hosted.runs().len(),
                1,
                "fresh identity distribution is ONE stride run per member"
            );
            let run = &st.slots_hosted.runs()[0];
            assert_eq!(usize::from(run.start), pos, "run starts at the position");
            assert_eq!(usize::from(run.stride), v.max(1), "stride = member count");
            assert!(
                st.slots_hosted.contains(pos as u16),
                "native slot hosted by its member"
            );
            total += st.slots_hosted.len() as u64;
            // The whole record — 5 roots + the stamp — encodes into one
            // 4096-B ledger slot with headroom.
            let image = rec(Some(st.clone()))
                .encode_slot()
                .expect("fresh stamp fits the ledger slot");
            assert_eq!(image.len() as u64, 4096);
        }
        assert_eq!(total, u64::from(W), "runs partition [0, W) exactly");
    }
}

#[test]
fn test_routing_round_trip_at_derived_width() {
    // Every slot, several locals — the ino arithmetic is unchanged; this
    // pins it at the derived magnitude (no overflow, exact inverse).
    for slot in [0u64, 1, 63, 64, 65535] {
        for local in [2u64, 3, 1 << 20, (1 << 40) - 1] {
            let ino = make_global_ino_width(local, slot, u64::from(W));
            let (s2, l2) = route_ino_width(ino, u64::from(W));
            assert_eq!((s2, l2), (slot, local), "round trip at W = 2^16");
        }
    }
    assert_eq!(route_ino_width(1, u64::from(W)), (0, 1), "root pin");
}

// ---------------------------------------------------------------------------
// SlotSet: stride runs
// ---------------------------------------------------------------------------

#[test]
fn test_slot_set_coalesces_and_mutates() {
    // A pure stride progression coalesces to one run.
    let evens: Vec<u16> = (0..100u16).map(|k| k * 2).collect();
    let set = SlotSet::from_slots(&evens);
    assert_eq!(set.runs().len(), 1, "one arithmetic run");
    assert_eq!(set.len(), 100);
    assert!(set.contains(84) && !set.contains(85));
    assert_eq!(set.iter().collect::<Vec<_>>(), evens);
    assert_eq!(set.smallest(), Some(0));

    // Removing an interior member splits bounded-ly; re-inserting heals.
    let mut set2 = set.clone();
    set2.remove(10);
    assert!(!set2.contains(10));
    assert_eq!(set2.len(), 99);
    assert!(
        set2.runs().len() <= 3,
        "one hole splits into at most 3 runs"
    );
    set2.insert(10);
    assert_eq!(set2.len(), 100);
    assert_eq!(set2.runs().len(), 1, "re-coalesced after heal");

    // Scattered singletons stay representable.
    let scattered = SlotSet::from_slots(&[1, 5, 6, 900, 65535]);
    assert_eq!(scattered.len(), 5);
    assert!(scattered.contains(65535));
    let empty = SlotSet::from_slots(&[]);
    assert!(empty.is_empty());
    assert_eq!(empty.smallest(), None);
}

#[test]
fn test_slot_set_from_runs_validation_refuses_malformed() {
    // Zero count refuses.
    assert!(
        SlotSet::from_runs(vec![SlotRun {
            start: 0,
            stride: 1,
            count: 0
        }])
        .is_err(),
        "count = 0 refuses"
    );
    // Zero stride with count > 1 refuses (degenerate repetition).
    assert!(
        SlotSet::from_runs(vec![SlotRun {
            start: 0,
            stride: 0,
            count: 2
        }])
        .is_err(),
        "stride = 0, count > 1 refuses"
    );
    // Out-of-range end refuses (start + (count−1)·stride > 65535).
    assert!(
        SlotSet::from_runs(vec![SlotRun {
            start: 2,
            stride: 2,
            count: 40000
        }])
        .is_err(),
        "run past the slot-id namespace refuses"
    );
    // Overlapping runs refuse.
    assert!(
        SlotSet::from_runs(vec![
            SlotRun {
                start: 0,
                stride: 2,
                count: 10
            },
            SlotRun {
                start: 4,
                stride: 4,
                count: 3
            },
        ])
        .is_err(),
        "overlapping runs refuse"
    );
    // A valid pair admits.
    let ok = SlotSet::from_runs(vec![
        SlotRun {
            start: 0,
            stride: 2,
            count: 10,
        },
        SlotRun {
            start: 1,
            stride: 2,
            count: 10,
        },
    ])
    .expect("disjoint runs admit");
    assert_eq!(ok.len(), 20);
}

proptest! {
    /// from_slots is a faithful set representation for arbitrary slot
    /// populations (the encode/decode carrier property).
    #[test]
    fn prop_slot_set_faithful(mut slots in proptest::collection::vec(0u16..=u16::MAX, 0..600)) {
        slots.sort_unstable();
        slots.dedup();
        let set = SlotSet::from_slots(&slots);
        prop_assert_eq!(set.len(), slots.len());
        prop_assert_eq!(set.iter().collect::<Vec<_>>(), slots.clone());
        for &s in &slots {
            prop_assert!(set.contains(s));
        }
    }
}

// ---------------------------------------------------------------------------
// Stamp wire v3: encode/decode + the encoding-budget caps
// ---------------------------------------------------------------------------

#[test]
fn test_stamp_round_trips_through_the_ledger_slot() {
    let mut st = fresh_stamp(1, 3);
    st.slot_cursors = (0..64u16)
        .map(|s| (s * 3 + 1, 1000 + u64::from(s)))
        .collect();
    st.native_slot = Some(1);
    let image = rec(Some(st.clone())).encode_slot().expect("encodes");
    let back = LedgerRecord::decode_slot(&image).expect("decodes");
    assert_eq!(
        back.membership_stamp.as_ref(),
        Some(&st),
        "stamp round-trips byte-faithfully (runs + cursors + native)"
    );
}

#[test]
fn test_encoding_budget_caps_encode_at_cap_refuse_past_it() {
    // Exactly STAMP_MAX_RUNS runs + STAMP_MAX_CURSORS cursors must
    // encode (the worst-case budget equation is load-bearing). Built
    // via `from_runs` (the decode path, which preserves runs verbatim)
    // because normalization would coalesce evenly-spaced singletons.
    let singleton_runs = |n: usize| {
        SlotSet::from_runs(
            (0..n)
                .map(|k| SlotRun {
                    start: (k * 5) as u16,
                    stride: 1,
                    count: 1,
                })
                .collect(),
        )
        .expect("disjoint singletons admit")
    };
    let mut st = fresh_stamp(0, 1);
    st.slots_hosted = singleton_runs(STAMP_MAX_RUNS);
    assert_eq!(st.slots_hosted.runs().len(), STAMP_MAX_RUNS);
    st.slot_cursors = (0..STAMP_MAX_CURSORS as u16).map(|s| (s, 7u64)).collect();
    st.native_slot = Some(0);
    rec(Some(st.clone()))
        .encode_slot()
        .expect("at-cap stamp encodes — the budget equation holds");

    // One more run refuses loud.
    let mut over_runs = st.clone();
    over_runs.slots_hosted = singleton_runs(STAMP_MAX_RUNS + 1);
    let err = rec(Some(over_runs))
        .encode_slot()
        .expect_err("cap+1 refuses");
    assert!(
        format!("{err}").contains("run"),
        "refusal names the run budget: {err}"
    );

    // One more cursor refuses loud.
    let mut over_cur = st;
    over_cur.slot_cursors.push((u16::MAX, 9));
    let err = rec(Some(over_cur))
        .encode_slot()
        .expect_err("cursor cap+1 refuses");
    assert!(
        format!("{err}").contains("cursor"),
        "refusal names the cursor budget: {err}"
    );
}

proptest! {
    /// Arbitrary within-cap stamps round-trip through the 4096-B slot.
    #[test]
    fn prop_stamp_round_trip(
        seed_slots in proptest::collection::vec(0u16..=u16::MAX, 1..200),
        cursors in proptest::collection::vec((0u16..=u16::MAX, 1u64..1 << 40), 0..64),
        epoch in 1u64..1 << 40,
        pos in 0u16..8,
    ) {
        let mut slots = seed_slots;
        slots.sort_unstable();
        slots.dedup();
        let set = SlotSet::from_slots(&slots);
        prop_assume!(set.runs().len() <= STAMP_MAX_RUNS);
        let mut cur = cursors;
        cur.sort_unstable_by_key(|(s, _)| *s);
        cur.dedup_by_key(|(s, _)| *s);
        let st = MembershipStamp {
            set_uuid: [9u8; 16],
            set_epoch: epoch,
            member_position: pos,
            member_count: 8,
            routing_width: W,
            slots_hosted: set,
            native_slot: Some(pos),
            slot_cursors: cur,
        };
        let image = rec(Some(st.clone())).encode_slot().expect("encodes");
        let back = LedgerRecord::decode_slot(&image).expect("decodes");
        prop_assert_eq!(back.membership_stamp, Some(st));
    }
}

// ---------------------------------------------------------------------------
// Mint spread: the width is REAL granularity, and cursors persist
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mint_spread_rotates_across_the_mint_set_and_survives_remount() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", VOL_LEN);
    format_set(std::slice::from_ref(&meta)).await;
    let uri = vec![meta.display().to_string()];

    let routed = open_routed_meta_set(&uri).await.expect("open routed set");
    assert_eq!(routed.routing_width(), u64::from(W));

    let mut slots = HashSet::new();
    let mut globals = HashSet::new();
    for _ in 0..(MINT_SPREAD * 4) {
        let (_local, global) = routed.allocate_local_ino(0).expect("mint");
        assert!(globals.insert(global), "global inos unique");
        slots.insert(routed.slot_of_ino(global));
    }
    assert_eq!(
        slots.len(),
        MINT_SPREAD,
        "minting rotates across exactly MINT_SPREAD slots — the granularity law"
    );
    shutdown_routed(&routed).await;

    // Remount: cursors persisted via the stamp; fresh mints never
    // collide with pre-remount globals.
    let routed2 = open_routed_meta_set(&uri).await.expect("re-open");
    for _ in 0..(MINT_SPREAD * 4) {
        let (_local, global) = routed2.allocate_local_ino(0).expect("mint after remount");
        assert!(
            globals.insert(global),
            "post-remount mint collided with a pre-remount global (cursor lost)"
        );
    }
    shutdown_routed(&routed2).await;
}

// ---------------------------------------------------------------------------
// Ino stability + routing equivalence across migration at the derived W
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_created_inos_stable_across_slot_migration_and_remount() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "meta0", VOL_LEN),
        make_file(dir.path(), "meta1", VOL_LEN),
    ];
    format_set(&metas).await;
    let uri = uris(&metas);

    let routed = open_routed_meta_set(&uri).await.expect("open");
    let mut made = Vec::new();
    for i in 0..96 {
        let ino = routed
            .create(1, &format!("f{i}"), libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create")
            .ino;
        made.push((format!("f{i}"), ino));
    }
    // Pick a LOADED slot on volume 0 that is not slot 0 (the root pin's
    // home) and migrate it to volume 1 — mint spread guarantees loaded
    // non-zero slots exist.
    let victim = made
        .iter()
        .map(|(_, ino)| routed.slot_of_ino(*ino))
        .find(|&s| s != 0 && routed.slot_map_snapshot()[s as usize] == 0)
        .expect("a loaded non-root slot on volume 0") as u16;
    squeezefs::meta_backend::slot_migration::migrate_slot(
        &routed,
        victim,
        1,
        &squeezefs::meta_backend::slot_migration::MigrationOptions::default(),
        &squeezefs::meta_backend::slot_migration::MigrationTestHooks::default(),
    )
    .await
    .expect("migrate a loaded slot at the derived width");
    assert_eq!(
        routed.slot_map_snapshot()[usize::from(victim)],
        1,
        "slot flipped to volume 1"
    );

    for (name, ino) in &made {
        let looked = routed.lookup(1, name).await.expect("lookup").ino;
        assert_eq!(looked, *ino, "st_ino stable across migration: {name}");
        routed
            .getattr(*ino)
            .await
            .expect("getattr routes post-flip");
    }
    shutdown_routed(&routed).await;

    let routed2 = open_routed_meta_set(&uri).await.expect("re-open post-flip");
    for (name, ino) in &made {
        assert_eq!(
            routed2.lookup(1, name).await.expect("lookup").ino,
            *ino,
            "st_ino stable across remount: {name}"
        );
    }
    shutdown_routed(&routed2).await;
}

// ---------------------------------------------------------------------------
// Bootstrap order-independence at the derived W
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_discovery_is_uri_order_independent_at_derived_width() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
        make_file(dir.path(), "m2", VOL_LEN),
    ];
    format_set(&metas).await;
    let forward = uris(&metas);
    let mut reversed = forward.clone();
    reversed.reverse();

    let d1 = discover_meta_set(&forward).await.expect("forward");
    let d2 = discover_meta_set(&reversed).await.expect("reversed");
    assert_eq!(d1.ordered_paths, d2.ordered_paths, "canonical order");
    assert_eq!(d1.routing_width, u64::from(W));
    assert_eq!(d1.slot_to_volume, d2.slot_to_volume, "identical map");
    assert_eq!(
        volume_set_generation(&forward).await.unwrap(),
        volume_set_generation(&reversed).await.unwrap(),
        "generation identical under any URI order"
    );
}

// ---------------------------------------------------------------------------
// `format --meta-slots` dies loud naming its successor
// ---------------------------------------------------------------------------

#[test]
fn test_meta_slots_flag_refuses_naming_successor() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_squeezefs"))
        .args([
            "format",
            "sqmeta:///tmp/nonexistent-dmr-meta",
            "sqdata:///tmp/nonexistent-dmr-data",
            "--meta-slots",
            "8",
        ])
        .output()
        .expect("run the binary");
    assert!(
        !out.status.success(),
        "--meta-slots must be a hard error (forward-only)"
    );
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("derived") || all.contains("dynamic"),
        "error names the successor (derived/dynamic routing): {all}"
    );
    assert!(
        all.contains("add-meta") || all.contains("migrate-meta-slot"),
        "error names the growth verbs: {all}"
    );
}
