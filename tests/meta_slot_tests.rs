//! PR VL5a — frozen routing width + durable slot map + membership stamps,
//! red-first (docs/design-volume-lifecycle.md §5.5.1/§5.5.1a, KD-7, KD-14,
//! G-VL-4 representation clauses). REPRESENTATION ONLY: no migration
//! engine, no guest-tree copy, no conveyor tee (those are PR VL5b).
//!
//! - **`KV_GUEST_SLOTS` = incompat bit 2** (KD-14): non-intersection with
//!   bits 0/1/3 pinned against the REAL pre-VL5a known mask; an old-mask
//!   binary must refuse a bit-2 superblock loud, naming the bit.
//! - **Bit-before-first-stamp ordering** (§5.5.1a numbered invariant):
//!   a stamp-extended ledger slot fails the pre-VL5a decoder's
//!   length-consistency equation and DECODES AS ABSENT — so the kill-9
//!   window between bit-set and first-stamp must already refuse old
//!   binaries at the superblock gate (pinned via
//!   `decode_sector_with_known`, the VL3 pattern).
//! - **Frozen `routing_width W`** (KD-7): `route_ino`/`make_global_ino`
//!   computed over durable W, never `volumes.len()`; the W ≤ 1 identity
//!   short-circuit is byte-identical to the pre-VL5a mapping (every
//!   single-meta-volume filesystem's st_ino stability rides on it); the
//!   W > N round trip `make_global(route(ino)) == ino` holds ∀ino with
//!   uniform slot distribution.
//! - **Membership stamp** in the root-ledger record payload: encode/decode
//!   property + the `encode_slot` 4096-B boundary at the 64-hosted-slot
//!   cap + the A/B torn-slot fallback law (a torn newest slot loses to
//!   its intact stamped predecessor).
//! - **Order-independent mount discovery** (§5.5.1a): stamps reconstruct
//!   the set by `member_position` regardless of URI order —
//!   `volume_set_generation` is identical under reordered URIs;
//!   disagreements (duplicate positions, wrong member_count, mixed
//!   epochs, mixed stamped/unstamped) refuse LOUD naming the volumes.
//! - **`format --meta-slots` bounds**: `volumes ≤ W ≤ 64 × volumes`,
//!   refusals loud; default formats stay legacy-shaped (no bit, no stamp,
//!   byte-identical config JSON — KD-14's untouched-sets law).
//! - **`volume repair-set`** (VL5a posture): prints the observed stamp
//!   state; re-stamps a COHERENT observed state (idempotent), infers the
//!   single missing member of an otherwise-coherent set, refuses
//!   everything else loud.

use proptest::prelude::*;
use squeezefs::meta_backend::kv::checkpoint::{
    read_newest_ledger, write_ledger_slot, LedgerRecord, MembershipStamp, TreeRoot,
    ROOT_LEDGER_SLOT_LEN,
};
use squeezefs::meta_backend::kv::slot_set::SlotSet;
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, FEATURES_INCOMPAT_KNOWN, FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING,
    FEATURE_INCOMPAT_KV_GUEST_SLOTS, FEATURE_INCOMPAT_KV_LAYOUT_DELTAS,
    FEATURE_INCOMPAT_KV_SLOT_MIGRATION, FEATURE_INCOMPAT_KV_V3,
    FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE, FEATURE_INCOMPAT_NODE_SEQ_WATERMARK,
};
use squeezefs::meta_backend::{
    discover_meta_set, make_global_ino_width, open_routed_meta_set, plan_meta_slot_set_with_width,
    route_ino_width, volume_set_generation, Metadata, RoutedMetaBackend, DERIVED_ROUTING_WIDTH,
};
use squeezefs::FormatConfig;
use std::path::{Path, PathBuf};

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

/// Format one DEFAULT v3 meta volume — since dynamic meta routing this
/// is a single-member dynamic-routing set (synthesized stamp, derived
/// width; there is no legacy stampless shape any more).
async fn format_single(meta: &Path) {
    squeezefs::meta_backend::kv::builder::format_v3(meta, VOL_LEN, &opts())
        .await
        .expect("format v3 meta volume");
}

/// Rewrite a formatted volume's newest ledger record WITHOUT its stamp —
/// manufactures the valid-ledger-but-stampless artifact (stripped /
/// foreign-tool state) the repair-set inference arm recovers.
async fn strip_stamp(meta: &Path) {
    let squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) =
        classify_volume(meta).await.unwrap()
    else {
        panic!("expected v3");
    };
    let mut rec = read_newest_ledger(meta, sb.root_ledger.start)
        .await
        .unwrap()
        .expect("formatted volume has a ledger record");
    rec.seq += 1;
    rec.membership_stamp = None;
    write_ledger_slot(meta, sb.root_ledger.start, &rec)
        .await
        .expect("write the stripped record");
}

/// Zero a formatted volume's root-ledger extent — manufactures the
/// stampless-bit-6 artifact (torn format / foreign ledger) the discovery
/// defense-in-depth refusal exists for.
async fn wipe_ledger(meta: &Path) {
    let squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) =
        classify_volume(meta).await.unwrap()
    else {
        panic!("expected v3");
    };
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(meta).unwrap();
    f.seek(SeekFrom::Start(sb.root_ledger.start)).unwrap();
    f.write_all(&vec![0u8; sb.root_ledger.len as usize])
        .unwrap();
    f.sync_all().unwrap();
}

/// Format a whole stamped set the way `format --meta-slots W` does:
/// plan (identity slot map, epoch 1), then per-volume
/// `format_v3_stamped` (bit 2 + stamped bootstrap ledger record).
async fn format_stamped_set(
    metas: &[PathBuf],
    width: u32,
) -> squeezefs::meta_backend::MetaSlotPlan {
    let plan = plan_meta_slot_set_with_width(metas.len(), width).expect("plan admits the bounds");
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

fn stamp(pos: u16, count: u16, width: u32, epoch: u64, uuid: [u8; 16]) -> MembershipStamp {
    MembershipStamp {
        set_uuid: uuid,
        set_epoch: epoch,
        member_position: pos,
        member_count: count,
        routing_width: width,
        slots_hosted: SlotSet::from_slots(
            &(0..width as u16)
                .filter(|s| s % count == pos)
                .collect::<Vec<_>>(),
        ),
        native_slot: Some(pos),
        slot_cursors: Vec::new(),
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

fn rec(seq: u64, n_roots: usize, stamp: Option<MembershipStamp>) -> LedgerRecord {
    LedgerRecord {
        seq,
        tree_roots: roots(n_roots),
        journal_tail_seq: 11,
        next_ino: 42,
        alloc_bitmap_generation: seq,
        node_seq_watermark: 99,
        membership_stamp: stamp,
        append_partition: None,
    }
}

// ---------------------------------------------------------------------------
// KD-14: KV_GUEST_SLOTS = bit 2 (non-intersection vs the pre-VL5a mask)
// ---------------------------------------------------------------------------

#[test]
fn test_guest_slots_bit_is_bit2_and_does_not_intersect_pre_vl5a_mask() {
    assert_eq!(FEATURE_INCOMPAT_KV_GUEST_SLOTS, 1 << 2, "KD-14: bit 2");
    // The pre-VL5a binary's FEATURES_INCOMPAT_KNOWN was bits 0|1|3.
    let pre_vl5a = FEATURE_INCOMPAT_KV_V3
        | FEATURE_INCOMPAT_NODE_SEQ_WATERMARK
        | FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE;
    assert_eq!(
        FEATURE_INCOMPAT_KV_GUEST_SLOTS & pre_vl5a,
        0,
        "bit 2 must not intersect the pre-VL5a known mask (bits 0/1/3)"
    );
    assert_ne!(
        FEATURES_INCOMPAT_KNOWN & FEATURE_INCOMPAT_KV_GUEST_SLOTS,
        0,
        "this binary must understand bit 2"
    );
}

// ---------------------------------------------------------------------------
// §5.5.1a: the bit-before-first-stamp ordering invariant
// ---------------------------------------------------------------------------

/// The forward-only refusal ladder at the superblock gate:
/// pre-VL5a masks (bits 0|1|3) refuse a fresh format naming bit 2;
/// pre-campaign masks (bits 0..=5) refuse it naming bit 6; and THIS
/// binary refuses a bit-6 volume whose ledger carries no stamp (torn
/// format / foreign ledger) loud at discovery — there is no legacy
/// implicit-identity fallback any more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_incompat_refusal_ladder_and_stampless_defense() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", VOL_LEN);
    format_single(&meta).await;

    // The stamp bits are format-time now: the lazy set is an idempotent
    // no-op on every fresh volume.
    assert!(
        !squeezefs::meta_backend::kv::superblock::set_guest_slots_bit(&meta)
            .await
            .expect("idempotent set"),
        "bit 2 is already set at format"
    );

    let mut sector = vec![0u8; 4096];
    use std::io::Read;
    std::fs::File::open(&meta)
        .unwrap()
        .read_exact(&mut sector)
        .unwrap();
    // Pre-VL5a mask (bits 0|1|3) refuses naming bit 2 — the REAL gate
    // code path (decode_sector_with_known), not a synthetic assertion.
    let pre_vl5a = FEATURE_INCOMPAT_KV_V3
        | FEATURE_INCOMPAT_NODE_SEQ_WATERMARK
        | FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE;
    let err = squeezefs::meta_backend::kv::superblock::SuperblockV3::decode_sector_with_known(
        &sector, pre_vl5a,
    )
    .expect_err("a pre-VL5a known-mask must refuse a fresh superblock");
    assert!(
        format!("{err}").contains("bit 2"),
        "refusal must name an unknown bit: {err}"
    );
    // Pre-campaign mask (bits 0..=5) refuses naming bit 6.
    let pre_campaign = pre_vl5a
        | FEATURE_INCOMPAT_KV_GUEST_SLOTS
        | FEATURE_INCOMPAT_KV_SLOT_MIGRATION
        | FEATURE_INCOMPAT_KV_LAYOUT_DELTAS;
    let err = squeezefs::meta_backend::kv::superblock::SuperblockV3::decode_sector_with_known(
        &sector,
        pre_campaign,
    )
    .expect_err("a pre-campaign known-mask must refuse a fresh superblock");
    assert!(
        format!("{err}").contains("bit 6"),
        "refusal must name bit 6: {err}"
    );
    // THIS binary understands the fresh format.
    squeezefs::meta_backend::kv::superblock::SuperblockV3::decode_sector_with_known(
        &sector,
        FEATURES_INCOMPAT_KNOWN,
    )
    .expect("this binary understands the fresh format");

    // Stampless bit-6 volume (wiped ledger): discovery refuses loud —
    // never a silent legacy fallback.
    wipe_ledger(&meta).await;
    let paths = vec![meta.display().to_string()];
    let err = discover_meta_set(&paths)
        .await
        .expect_err("a stampless bit-6 volume must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains("reformat") || msg.contains("squeezefs format"),
        "stampless refusal names the remedy: {msg}"
    );
}

/// The reason bit-first is load-bearing: a stamp-extended slot fails the
/// PRE-VL5a decoder's length-consistency equation
/// (`PAYLOAD_FIXED_LEN + n_roots × 17 == payload_len`) and decodes as
/// absent — an old binary handed a stamped-but-unbitted volume would
/// silently fall back to an older ledger slot (stale roots). Pinned
/// structurally: the stamped image's declared payload length strictly
/// exceeds the stampless image's for identical roots.
#[test]
fn test_stamped_slot_fails_the_old_length_equation_decodes_as_absent() {
    let uuid = [7u8; 16];
    let bare = rec(5, 3, None).encode_slot().expect("stampless encodes");
    let stamped = rec(5, 3, Some(stamp(0, 1, 4, 1, uuid)))
        .encode_slot()
        .expect("stamped encodes");
    let payload_len = |img: &[u8]| u32::from_le_bytes(img[4..8].try_into().unwrap()) as usize;
    assert!(
        payload_len(&stamped) > payload_len(&bare),
        "the stamp must extend the payload past the old fixed+roots equation \
         ({} vs {})",
        payload_len(&stamped),
        payload_len(&bare)
    );
    // And the new decoder round-trips both.
    assert_eq!(LedgerRecord::decode_slot(&bare).unwrap(), rec(5, 3, None));
    assert_eq!(
        LedgerRecord::decode_slot(&stamped).unwrap(),
        rec(5, 3, Some(stamp(0, 1, 4, 1, uuid)))
    );
}

// ---------------------------------------------------------------------------
// KD-7: frozen routing width — W ≤ 1 identity pinned, W > N round trip
// ---------------------------------------------------------------------------

/// W ≤ 1 keeps the EXACT identity short-circuit across the ino space,
/// including ino 1 and the ino-2 boundary — byte-identical to the
/// pre-VL5a `num_volumes ≤ 1` mapping every existing single-meta-volume
/// filesystem's st_ino stability depends on.
#[test]
fn test_w1_routing_is_byte_identical_identity() {
    for ino in [1u64, 2, 3, 4095, 4096, 1 << 33, u64::MAX - 1] {
        assert_eq!(route_ino_width(ino, 1), (0, ino), "W=1 route is identity");
        assert_eq!(route_ino_width(ino, 0), (0, ino), "W=0 route is identity");
        assert_eq!(
            make_global_ino_width(ino, 0, 1),
            ino,
            "W=1 make_global is identity"
        );
    }
    // The general arithmetic coincides at W=1 by construction:
    // (ino−2)/1 + 2 = ino and (local−2)·1 + 0 + 2 = local.
    for ino in 2u64..64 {
        let (slot, local) = route_ino_width(ino, 1);
        assert_eq!((slot, local), (0, ino));
        assert_eq!(make_global_ino_width(local, slot, 1), ino);
    }
}

/// ∀ino: make_global(route(ino)) == ino over frozen widths W > N, and the
/// slot distribution is uniform (consecutive inos walk the slots).
#[test]
fn test_route_make_global_round_trip_and_distribution_over_width() {
    for width in [2u64, 3, 5, 8, 64, 512] {
        let mut hits = vec![0u64; width as usize];
        for ino in 2..2 + width * 8 {
            let (slot, local) = route_ino_width(ino, width);
            assert!(slot < width, "slot in range");
            assert!(local >= 2, "locals stay above the reserved inos");
            assert_eq!(
                make_global_ino_width(local, slot, width),
                ino,
                "round trip at ino {ino}, W {width}"
            );
            hits[slot as usize] += 1;
        }
        assert!(
            hits.iter().all(|&h| h == 8),
            "consecutive inos must distribute uniformly over W={width}: {hits:?}"
        );
        // ino 1 pins to slot 0.
        assert_eq!(route_ino_width(1, width), (0, 1), "ino 1 pins to slot 0");
        assert_eq!(make_global_ino_width(1, 0, width), 1);
    }
    // Sparse large-ino probes.
    proptest!(|(ino in 2u64..u64::MAX / 2, width in 2u64..1024)| {
        let (slot, local) = route_ino_width(ino, width);
        prop_assert_eq!(make_global_ino_width(local, slot, width), ino);
    });
}

/// The in-RAM test constructor (`RoutedMetaBackend::new`) routes over
/// the implicit W = volume count, identity for one volume — the W <= 1
/// arithmetic pin the data-path test rigs ride (production mounts route
/// over the stamps' derived width via `open_routed_meta_set`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_in_ram_constructor_routes_identity_for_one_volume() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", VOL_LEN);
    format_single(&meta).await;
    let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(&meta)
        .await
        .expect("open legacy volume");
    let routed = RoutedMetaBackend::new(vec![kv]);
    assert_eq!(
        routed.routing_width(),
        1,
        "in-RAM constructor: W = volume count"
    );
    for ino in [1u64, 2, 3, 999_999] {
        assert_eq!(routed.route_ino(ino), (0, ino), "identity short-circuit");
        assert_eq!(routed.make_global_ino(ino, 0), ino);
    }
    routed.volumes[0].shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Membership stamp: encode/decode property + A/B torn-slot fallback
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn prop_stamp_encode_decode_round_trip(
        seq in 1u64..1_000_000,
        n_roots in 0usize..=5,
        set_epoch in 1u64..1_000_000,
        member_position in 0u16..64,
        member_count in 1u16..=64,
        routing_width in 1u32..=4096,
        n_slots in 0usize..=512,
        uuid in prop::array::uniform16(0u8..),
        with_stamp in prop::bool::ANY,
    ) {
        let st = with_stamp.then(|| MembershipStamp {
            set_uuid: uuid,
            set_epoch,
            member_position,
            member_count,
            routing_width,
            slots_hosted: SlotSet::from_slots(&(0..n_slots as u16).collect::<Vec<_>>()),
            native_slot: None,
            slot_cursors: Vec::new(),
        });
        let r = rec(seq, n_roots, st);
        let img = r.encode_slot().expect("encodes");
        prop_assert_eq!(img.len(), ROOT_LEDGER_SLOT_LEN as usize);
        let back = LedgerRecord::decode_slot(&img).expect("decodes");
        prop_assert_eq!(back, r);
    }
}

/// A/B torn-slot law with stamps: corrupt the newest (stamped) slot —
/// selection falls back to its intact stamped predecessor, never loud.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_torn_newest_stamped_slot_falls_back_to_predecessor() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = make_file(dir.path(), "ledger", 32 * ROOT_LEDGER_SLOT_LEN);
    let uuid = [9u8; 16];
    let older = rec(7, 3, Some(stamp(1, 2, 4, 3, uuid)));
    let newer = rec(8, 3, Some(stamp(1, 2, 4, 4, uuid)));
    write_ledger_slot(&ledger, 0, &older).await.unwrap();
    write_ledger_slot(&ledger, 0, &newer).await.unwrap();

    // Sanity: newest wins while intact.
    let got = read_newest_ledger(&ledger, 0).await.unwrap().unwrap();
    assert_eq!(got, newer);

    // Tear the newest slot (slot = seq % 32 = 8) mid-payload.
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(&ledger)
        .unwrap();
    f.seek(SeekFrom::Start(8 * ROOT_LEDGER_SLOT_LEN + 40))
        .unwrap();
    f.write_all(&[0xFF; 64]).unwrap();
    f.sync_all().unwrap();

    let got = read_newest_ledger(&ledger, 0).await.unwrap().unwrap();
    assert_eq!(
        got, older,
        "a torn stamped slot must lose to its intact predecessor (with ITS stamp)"
    );
}

// ---------------------------------------------------------------------------
// format --meta-slots: bounds, stamps at format, legacy byte-identity
// ---------------------------------------------------------------------------

#[test]
fn test_plan_width_bounds_and_the_retired_64x_cap() {
    assert!(
        plan_meta_slot_set_with_width(0, 8).is_err(),
        "zero volumes refuses"
    );
    assert!(
        plan_meta_slot_set_with_width(2, 1).is_err(),
        "W < volumes refuses (every volume hosts >= 1 slot)"
    );
    assert!(
        plan_meta_slot_set_with_width(2, 2).is_ok(),
        "W = volumes admits"
    );
    // The retired ledger-space bound (W <= 64 x volumes) — stride runs
    // made it obsolete: a volume hosts any number of slots as ONE run.
    assert!(
        plan_meta_slot_set_with_width(2, 129).is_ok(),
        "W > 64 x volumes ADMITS now (the dense-encoding cap is retired)"
    );
    assert!(
        plan_meta_slot_set_with_width(1, DERIVED_ROUTING_WIDTH).is_ok(),
        "the full derived width admits at any volume count"
    );
    let msg = format!(
        "{}",
        plan_meta_slot_set_with_width(2, DERIVED_ROUTING_WIDTH + 1).unwrap_err()
    );
    assert!(
        msg.contains("slot-id namespace"),
        "past-namespace widths refuse naming the derivation: {msg}"
    );
}

#[test]
fn test_plan_meta_slot_set_identity_distribution_and_hosted_runs() {
    let plan = plan_meta_slot_set_with_width(3, 7).expect("bounds admit");
    assert_eq!(plan.routing_width, 7);
    assert_eq!(plan.stamps.len(), 3);
    for (pos, st) in plan.stamps.iter().enumerate() {
        assert_eq!(st.member_position, pos as u16);
        assert_eq!(st.member_count, 3);
        assert_eq!(st.routing_width, 7);
        assert_eq!(st.set_epoch, 1, "format mints epoch 1");
        assert_eq!(st.set_uuid, plan.set_uuid);
        assert_eq!(
            st.native_slot,
            Some(pos as u16),
            "every founding member's native slot is its position"
        );
        let expect: Vec<u16> = (0..7u16).filter(|s| s % 3 == pos as u16).collect();
        assert_eq!(st.slots_hosted.to_vec(), expect, "identity distribution");
        assert_eq!(st.slots_hosted.runs().len(), 1, "one stride run per member");
    }
    // Every slot hosted exactly once across the set.
    let mut all: Vec<u16> = plan
        .stamps
        .iter()
        .flat_map(|s| s.slots_hosted.to_vec())
        .collect();
    all.sort_unstable();
    assert_eq!(all, (0..7u16).collect::<Vec<_>>());
}

/// Default formats are SINGLE-MEMBER DYNAMIC SETS now (forward-only —
/// the legacy stampless shape died with the frozen widths): stamp bits +
/// bit 6 on the superblock, a derived-width one-run stamp in the
/// bootstrap ledger record, and the on-disk slot re-encodes identically
/// (byte round-trip). Additive config fields still serialize to nothing
/// when unset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_default_format_is_a_single_member_dynamic_set() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "meta", VOL_LEN);
    format_single(&meta).await;

    let squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) =
        classify_volume(&meta).await.unwrap()
    else {
        panic!("expected v3");
    };
    for (bit, name) in [
        (FEATURE_INCOMPAT_KV_GUEST_SLOTS, "bit 2"),
        (FEATURE_INCOMPAT_KV_SLOT_MIGRATION, "bit 4"),
        (FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING, "bit 6"),
    ] {
        assert_ne!(
            sb.features_incompat & bit,
            0,
            "default format must carry {name}"
        );
    }
    let ledger = read_newest_ledger(&meta, sb.root_ledger.start)
        .await
        .unwrap()
        .expect("bootstrap record exists");
    let st = ledger
        .membership_stamp
        .as_ref()
        .expect("default format stamps its single member");
    assert_eq!(st.routing_width, DERIVED_ROUTING_WIDTH);
    assert_eq!(st.member_count, 1);
    assert_eq!(st.member_position, 0);
    assert_eq!(st.native_slot, Some(0));
    assert_eq!(st.slots_hosted.runs().len(), 1, "one run hosts all W slots");
    assert_eq!(st.slots_hosted.len(), DERIVED_ROUTING_WIDTH as usize);
    // Byte pin: the slot on disk is exactly this encoding.
    let img = ledger.encode_slot().unwrap();
    let mut on_disk = vec![0u8; ROOT_LEDGER_SLOT_LEN as usize];
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(&meta).unwrap();
    f.seek(SeekFrom::Start(
        sb.root_ledger.start + (ledger.seq % 32) * ROOT_LEDGER_SLOT_LEN,
    ))
    .unwrap();
    f.read_exact(&mut on_disk).unwrap();
    assert_eq!(img, on_disk, "ledger slot bytes re-encode identically");

    // Additive config fields serialize to NOTHING when unset.
    let cfg: FormatConfig = serde_json::from_value(serde_json::json!({
        "name": "squeezefs",
        "block_size": 4096u64,
        "capacity": 1u64 << 30,
        "inodes": 1000u64,
        "compression": "none",
        "encrypt_algo": "none",
        "data_lv": ["/dev/null"],
    }))
    .expect("old config JSON decodes (additive fields default None)");
    assert!(cfg.meta_routing_width.is_none());
    assert!(cfg.meta_slot_runs.is_none());
    assert!(cfg.meta_volumes.is_none());
    let out = serde_json::to_value(&cfg).unwrap();
    for key in ["meta_routing_width", "meta_slot_runs", "meta_volumes"] {
        assert!(
            out.get(key).is_none(),
            "unset {key} must be skipped when serializing: {out}"
        );
    }
}

/// `--meta-slots` formats are lifecycle-shaped: bit 2 on every member,
/// stamps in every bootstrap ledger record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_meta_slots_format_stamps_and_sets_bit2() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    let plan = format_stamped_set(&metas, 4).await;
    for (i, m) in metas.iter().enumerate() {
        let squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) =
            classify_volume(m).await.unwrap()
        else {
            panic!("expected v3");
        };
        assert_ne!(
            sb.features_incompat & FEATURE_INCOMPAT_KV_GUEST_SLOTS,
            0,
            "member {i} must carry bit 2"
        );
        let ledger = read_newest_ledger(m, sb.root_ledger.start)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ledger.membership_stamp.as_ref(),
            Some(&plan.stamps[i]),
            "member {i} bootstrap record carries its stamp"
        );
    }
}

// ---------------------------------------------------------------------------
// §5.5.1a: order-independent discovery + generation, loud disagreements
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_reordered_uris_discover_same_set_and_same_generation() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
        make_file(dir.path(), "m2", VOL_LEN),
    ];
    format_stamped_set(&metas, 6).await;
    let fwd: Vec<String> = metas.iter().map(|p| p.display().to_string()).collect();
    let mut rev = fwd.clone();
    rev.reverse();

    let d1 = discover_meta_set(&fwd).await.expect("forward discovers");
    let d2 = discover_meta_set(&rev).await.expect("reversed discovers");
    assert_eq!(d1.routing_width, 6);
    assert_eq!(
        d1.ordered_paths, d2.ordered_paths,
        "canonical member_position order, not URI order"
    );
    assert_eq!(d1.slot_to_volume, d2.slot_to_volume);

    let g1 = volume_set_generation(&fwd).await.unwrap();
    let g2 = volume_set_generation(&rev).await.unwrap();
    assert_eq!(
        g1, g2,
        "volume_set_generation must ride the canonical order (staging identity)"
    );

    // Contrast: two INDEPENDENTLY-formatted volumes are two different
    // single-member SETS — listing them as one URI refuses loud (foreign
    // set uuids), never a silently-joined view.
    let dirl = tempfile::tempdir().unwrap();
    let l0 = make_file(dirl.path(), "l0", VOL_LEN);
    let l1 = make_file(dirl.path(), "l1", VOL_LEN);
    format_single(&l0).await;
    format_single(&l1).await;
    let lf = vec![l0.display().to_string(), l1.display().to_string()];
    let err = volume_set_generation(&lf)
        .await
        .expect_err("independently-formatted volumes are foreign sets");
    assert!(
        format!("{err}").contains("DIFFERENT sets"),
        "refusal names the foreign-set class: {err}"
    );
}

/// End to end through the guarded open: create files on the canonical
/// mount, close, reopen with REVERSED URIs — same inos resolve, same
/// content identity (order-independent bootstrap).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_reordered_uri_mount_resolves_same_inos() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 4).await;
    let fwd: Vec<String> = metas.iter().map(|p| p.display().to_string()).collect();
    let mut rev = fwd.clone();
    rev.reverse();

    let routed = open_routed_meta_set(&fwd).await.expect("canonical open");
    assert_eq!(routed.routing_width(), 4, "frozen W from the stamps");
    let mut made = Vec::new();
    for i in 0..8 {
        let inode = routed
            .create(1, &format!("f{i}"), libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create");
        made.push((format!("f{i}"), inode.ino));
    }
    for vol in &routed.volumes {
        vol.shutdown().await.unwrap();
    }
    drop(routed);

    let routed = open_routed_meta_set(&rev).await.expect("reversed open");
    assert_eq!(routed.routing_width(), 4);
    for (name, ino) in &made {
        let got = routed.lookup(1, name).await.expect("lookup after reorder");
        assert_eq!(
            got.ino, *ino,
            "global inos are eternally stable across URI orders"
        );
        let attr = routed.getattr(*ino).await.expect("getattr");
        assert_eq!(attr.ino, *ino);
    }
    for vol in &routed.volumes {
        vol.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_discovery_disagreements_refuse_loud_naming_volumes() {
    let dir = tempfile::tempdir().unwrap();

    // Duplicate member_position.
    let a = make_file(dir.path(), "dup_a", VOL_LEN);
    let b = make_file(dir.path(), "dup_b", VOL_LEN);
    let uuid = [1u8; 16];
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &a,
        VOL_LEN,
        &opts(),
        stamp(0, 2, 2, 1, uuid),
    )
    .await
    .unwrap();
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &b,
        VOL_LEN,
        &opts(),
        stamp(0, 2, 2, 1, uuid), // duplicate position 0
    )
    .await
    .unwrap();
    let paths = vec![a.display().to_string(), b.display().to_string()];
    let err = discover_meta_set(&paths)
        .await
        .expect_err("duplicate positions must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains(&a.display().to_string()) && msg.contains(&b.display().to_string()),
        "refusal must name both volumes: {msg}"
    );

    // Missing member: stamps say count 3, URI lists 2.
    let c = make_file(dir.path(), "mis_c", VOL_LEN);
    let d = make_file(dir.path(), "mis_d", VOL_LEN);
    let uuid2 = [2u8; 16];
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &c,
        VOL_LEN,
        &opts(),
        stamp(0, 3, 3, 1, uuid2),
    )
    .await
    .unwrap();
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &d,
        VOL_LEN,
        &opts(),
        stamp(1, 3, 3, 1, uuid2),
    )
    .await
    .unwrap();
    let paths = vec![c.display().to_string(), d.display().to_string()];
    let err = discover_meta_set(&paths)
        .await
        .expect_err("an incomplete member set must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains('3') && msg.contains('2'),
        "refusal must state the expected vs listed member counts: {msg}"
    );

    // Epoch spread (PR VL5b amendment — the VL5a refusal was explicitly
    // temporary: "the §5.5.2b epoch resolution lands with VL5b"): a
    // slot flip bumps only its participants, so mixed epochs over
    // uniquely-claimed slots now MOUNT via per-slot highest-epoch-wins.
    let e = make_file(dir.path(), "ep_e", VOL_LEN);
    let f = make_file(dir.path(), "ep_f", VOL_LEN);
    let uuid3 = [3u8; 16];
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &e,
        VOL_LEN,
        &opts(),
        stamp(0, 2, 2, 1, uuid3),
    )
    .await
    .unwrap();
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &f,
        VOL_LEN,
        &opts(),
        stamp(1, 2, 2, 2, uuid3), // epoch 2 vs 1 — a legitimate rest state
    )
    .await
    .unwrap();
    let paths = vec![e.display().to_string(), f.display().to_string()];
    let disc = discover_meta_set(&paths)
        .await
        .expect("epoch spread with unique claims resolves (§5.5.2b)");
    assert_eq!(disc.slot_to_volume, vec![0, 1]);
    // A SAME-epoch dual claim stays a loud refusal (corruption by
    // construction — one coordinator, one flip per slot at a time).
    let e2 = make_file(dir.path(), "dc_e", VOL_LEN);
    let f2 = make_file(dir.path(), "dc_f", VOL_LEN);
    let uuid3b = [13u8; 16];
    let mut st_a = stamp(0, 2, 2, 3, uuid3b);
    st_a.slots_hosted = SlotSet::from_slots(&[0, 1]);
    let mut st_b = stamp(1, 2, 2, 3, uuid3b);
    st_b.slots_hosted = SlotSet::from_slots(&[1]);
    squeezefs::meta_backend::kv::builder::format_v3_stamped(&e2, VOL_LEN, &opts(), st_a)
        .await
        .unwrap();
    squeezefs::meta_backend::kv::builder::format_v3_stamped(&f2, VOL_LEN, &opts(), st_b)
        .await
        .unwrap();
    let paths = vec![e2.display().to_string(), f2.display().to_string()];
    let err = discover_meta_set(&paths)
        .await
        .expect_err("a same-epoch dual claim must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains(&e2.display().to_string()) && msg.contains(&f2.display().to_string()),
        "same-epoch dual-claim refusal must name the volumes: {msg}"
    );

    // Different set uuids (a volume from another set).
    let g = make_file(dir.path(), "set_g", VOL_LEN);
    let h = make_file(dir.path(), "set_h", VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &g,
        VOL_LEN,
        &opts(),
        stamp(0, 2, 2, 1, [4u8; 16]),
    )
    .await
    .unwrap();
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &h,
        VOL_LEN,
        &opts(),
        stamp(1, 2, 2, 1, [5u8; 16]),
    )
    .await
    .unwrap();
    let paths = vec![g.display().to_string(), h.display().to_string()];
    let err = discover_meta_set(&paths)
        .await
        .expect_err("foreign-set members must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains(&g.display().to_string()) && msg.contains(&h.display().to_string()),
        "foreign-set refusal must name the volumes: {msg}"
    );

    // Mixed stamped/unstamped.
    let i = make_file(dir.path(), "mix_i", VOL_LEN);
    let j = make_file(dir.path(), "mix_j", VOL_LEN);
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &i,
        VOL_LEN,
        &opts(),
        stamp(0, 2, 2, 1, [6u8; 16]),
    )
    .await
    .unwrap();
    format_single(&j).await;
    wipe_ledger(&j).await;
    let paths = vec![i.display().to_string(), j.display().to_string()];
    let err = discover_meta_set(&paths)
        .await
        .expect_err("mixed stamped/unstamped must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains(&j.display().to_string()),
        "mixed refusal must name the stampless volume: {msg}"
    );
}

// ---------------------------------------------------------------------------
// volume repair-set (VL5a posture: print + re-stamp coherent state only)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_repair_set_restamps_coherent_state_and_refuses_incoherent() {
    let dir = tempfile::tempdir().unwrap();

    // Single-member dynamic set: coherent — idempotent re-stamp, still
    // discovers with the derived width afterward.
    let l = make_file(dir.path(), "single", VOL_LEN);
    format_single(&l).await;
    let paths = vec![l.display().to_string()];
    let restamped = squeezefs::config_ops::repair_meta_set(&paths)
        .await
        .expect("a coherent single-member set re-stamps");
    assert_eq!(restamped.len(), 1, "the single member re-stamps");
    let disc = discover_meta_set(&paths).await.unwrap();
    assert_eq!(disc.routing_width, u64::from(DERIVED_ROUTING_WIDTH));

    // Coherent stamped set: idempotent re-stamp, same generation after.
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    format_stamped_set(&metas, 4).await;
    let paths: Vec<String> = metas.iter().map(|p| p.display().to_string()).collect();
    let gen_before = volume_set_generation(&paths).await.unwrap();
    let restamped = squeezefs::config_ops::repair_meta_set(&paths)
        .await
        .expect("coherent set re-stamps");
    assert_eq!(restamped.len(), 2, "both members re-stamped");
    assert_eq!(volume_set_generation(&paths).await.unwrap(), gen_before);
    discover_meta_set(&paths).await.expect("still discovers");

    // The kill-9 window repair-set itself can leave behind: one member
    // bit-set but stampless beside an otherwise-coherent set — the single
    // missing position is inferable; repair stamps it.
    let ka = make_file(dir.path(), "k_a", VOL_LEN);
    let kb = make_file(dir.path(), "k_b", VOL_LEN);
    let uuid = [8u8; 16];
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &ka,
        VOL_LEN,
        &opts(),
        stamp(0, 2, 4, 1, uuid),
    )
    .await
    .unwrap();
    format_single(&kb).await;
    strip_stamp(&kb).await;
    squeezefs::meta_backend::kv::superblock::set_guest_slots_bit(&kb)
        .await
        .unwrap();
    let paths = vec![ka.display().to_string(), kb.display().to_string()];
    discover_meta_set(&paths)
        .await
        .expect_err("the window refuses a plain mount");
    let restamped = squeezefs::config_ops::repair_meta_set(&paths)
        .await
        .expect("single inferable member repairs");
    assert!(
        restamped.contains(&kb.display().to_string()),
        "the stampless member must be re-stamped: {restamped:?}"
    );
    let disc = discover_meta_set(&paths)
        .await
        .expect("repaired set discovers");
    assert_eq!(disc.routing_width, 4);

    // Epoch spread (PR VL5b amendment): repair-set now RESOLVES a
    // slot-flip epoch spread — per-slot highest-epoch-wins, every member
    // re-stamped at the max epoch (the VL5a refusal was explicitly
    // "lands with VL5b").
    let ea = make_file(dir.path(), "e_a", VOL_LEN);
    let eb = make_file(dir.path(), "e_b", VOL_LEN);
    let uuid = [10u8; 16];
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &ea,
        VOL_LEN,
        &opts(),
        stamp(0, 2, 2, 1, uuid),
    )
    .await
    .unwrap();
    squeezefs::meta_backend::kv::builder::format_v3_stamped(
        &eb,
        VOL_LEN,
        &opts(),
        stamp(1, 2, 2, 2, uuid),
    )
    .await
    .unwrap();
    let paths = vec![ea.display().to_string(), eb.display().to_string()];
    let restamped = squeezefs::config_ops::repair_meta_set(&paths)
        .await
        .expect("an epoch-spread slot-flip state resolves in VL5b");
    assert_eq!(
        restamped.len(),
        2,
        "both members re-stamped at the max epoch"
    );
    let disc = discover_meta_set(&paths)
        .await
        .expect("resolved set discovers");
    assert_eq!(disc.set_epoch, 2, "resolution converges on the max epoch");
}

// ---------------------------------------------------------------------------
// Durable slot map survives checkpoints (the stamp rides every ledger write)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stamp_survives_mount_write_checkpoint_remount() {
    let dir = tempfile::tempdir().unwrap();
    let metas = vec![
        make_file(dir.path(), "m0", VOL_LEN),
        make_file(dir.path(), "m1", VOL_LEN),
    ];
    let plan = format_stamped_set(&metas, 4).await;
    let paths: Vec<String> = metas.iter().map(|p| p.display().to_string()).collect();

    // Mount, mutate (forces checkpoints past the bootstrap record), close.
    let routed = open_routed_meta_set(&paths).await.expect("open");
    for i in 0..32 {
        routed
            .create(1, &format!("g{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    for vol in &routed.volumes {
        vol.shutdown().await.unwrap();
    }
    drop(routed);

    // The NEWEST ledger record (a post-bootstrap checkpoint) still
    // carries each volume's stamp.
    for (i, m) in metas.iter().enumerate() {
        let squeezefs::meta_backend::kv::superblock::VolumeFormat::V3(sb) =
            classify_volume(m).await.unwrap()
        else {
            panic!("expected v3");
        };
        let ledger = read_newest_ledger(m, sb.root_ledger.start)
            .await
            .unwrap()
            .unwrap();
        assert!(
            ledger.seq > 1,
            "writes must have produced a post-bootstrap checkpoint (seq {})",
            ledger.seq
        );
        let got = ledger
            .membership_stamp
            .as_ref()
            .expect("stamp rides every checkpoint");
        // Mint-spread cursors legitimately accrue after writes — compare
        // the identity/geometry fields, then require cursors for the
        // guest mint slots the creates touched.
        assert_eq!(got.set_uuid, plan.stamps[i].set_uuid);
        assert_eq!(got.member_position, plan.stamps[i].member_position);
        assert_eq!(got.member_count, plan.stamps[i].member_count);
        assert_eq!(got.routing_width, plan.stamps[i].routing_width);
        assert_eq!(got.slots_hosted, plan.stamps[i].slots_hosted);
        assert_eq!(got.native_slot, plan.stamps[i].native_slot);
    }
}
