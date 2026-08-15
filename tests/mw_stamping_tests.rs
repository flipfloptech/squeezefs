//! PR 5 — `feat/mw-stamping` (docs/design-full-multi-writer.md §6, KD-MW-1):
//! the nine multi-writer incompat bits (7,8,9,10,11,12,13,14,15) stamp as
//! **ONE act**, never piecemeal.
//!
//! Contracts pinned here BEFORE any implementation:
//!
//! - **Phase A (dark opt-in)**: `format --multi-writer` stamps all nine at
//!   plan time (one plan, one superblock write); the DEFAULT format stays
//!   today's posture exactly (bits 0/1/6/7 — the Phase-B flip is rung 10b's,
//!   deliberately NOT this rung's).
//! - **`volume enable-multi-writer`** (offline, D0-guarded, the add-meta
//!   posture): per-volume bit order 7→9→15→12→13→8→10→14→11 (13 after 7 per
//!   its own refusal law; 11 deliberately TERMINAL), each stamp barriered,
//!   idempotent, crash-resumable.
//! - **The `mw_upgrade:` intent marker** (§6.2 mechanism i): the verb's
//!   FIRST act writes one marker record on ino 1 of volume 0 (the KD-2
//!   plane) naming the target bit set + volume list; its LAST act deletes
//!   it; a writable mount refuses while it exists. The enable verb's own
//!   guarded open is the ONE marker-tolerant writable open.
//! - **The bit-11 uniformity invariant** (§6.2 mechanism ii): writable
//!   mounts refuse iff the marker exists, OR bit-11 presence differs across
//!   the set (shape a), OR any volume carries bit 11 without the other
//!   eight (the foreign-tool tripwire — refusal names fsck). Shape (c)
//!   legitimately-partial populations (standalone 7/15, bit-5 runtime
//!   stamps, …) must NOT trip — grandfathered.
//! - **The two §6.2 interaction rules**: `add-meta` onto a bit-11-uniform
//!   set stamps the new member to match (fresh-format arm, no marker);
//!   the converse — a bit-11 volume joining a non-upgraded set — refuses.
//! - **Crash windows** (§10): MW-S1 (between volumes), MW-S1b (between
//!   EVERY adjacent bit pair — 8 windows + the marker-alone window),
//!   MW-S2 (bit set, structure unminted — first-mount minting is the
//!   existing crash-safe machinery), MW-S3 (old binary refuses loud via
//!   `FEATURES_INCOMPAT_KNOWN`).
//! - **Serialization**: the verb asserts the D0 guard before its first
//!   write; a second concurrent invocation refuses on the guard — extended
//!   to cover the marker-tolerant resume open.

use std::path::{Path, PathBuf};

use squeezefs::config_ops::{
    add_meta_volume, enable_multi_writer, enable_multi_writer_with, EnableMwCrash, EnableMwHooks,
    TakeSlots,
};
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{
    format_v3, format_v3_multi_writer, format_v3_stamped, FormatV3Options,
};
use squeezefs::meta_backend::kv::superblock as sb;
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, VolumeFormat, FEATURES_INCOMPAT_KNOWN, MULTI_WRITER_FORMAT_BITS,
};
use squeezefs::meta_backend::{
    open_routed_meta_set, open_volume_probe, plan_meta_slot_set, Metadata,
};
use squeezefs::MW_UPGRADE_MARKER_XATTR;

const VOL_LEN: u64 = 128 * 1024 * 1024;

/// The §6.2 per-volume stamp order (dependencies first, bit 11 terminal).
const ENABLE_ORDER: [u64; 9] = [7, 9, 15, 12, 13, 8, 10, 14, 11];

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

fn make_file(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    p
}

fn uris(metas: &[PathBuf]) -> Vec<String> {
    metas.iter().map(|p| p.display().to_string()).collect()
}

/// Format an n-member DEFAULT-posture set (today's fresh-format bits).
async fn format_default_set(metas: &[PathBuf]) {
    let plan = plan_meta_slot_set(metas.len()).expect("plan");
    for (i, m) in metas.iter().enumerate() {
        format_v3_stamped(m, VOL_LEN, &opts(), plan.stamps[i].clone())
            .await
            .expect("format default member");
    }
}

/// Format an n-member set through the Phase-A `--multi-writer` arm.
async fn format_mw_set(metas: &[PathBuf]) {
    let plan = plan_meta_slot_set(metas.len()).expect("plan");
    for (i, m) in metas.iter().enumerate() {
        squeezefs::meta_backend::kv::builder::format_v3_stamped_multi_writer(
            m,
            VOL_LEN,
            &opts(),
            plan.stamps[i].clone(),
        )
        .await
        .expect("format mw member");
    }
}

async fn features_of(path: &Path) -> u64 {
    match classify_volume(path).await.expect("classify") {
        VolumeFormat::V3(sb) => sb.features_incompat,
        other => panic!("expected a v3 volume, got {other:?}"),
    }
}

fn has_all_nine(features: u64) -> bool {
    features & MULTI_WRITER_FORMAT_BITS == MULTI_WRITER_FORMAT_BITS
}

fn has_none_beyond_default(features: u64) -> bool {
    // Today's fresh-format posture is bits 0/1/6/7 (+2/4 on stamped set
    // members): of the mw set only bit 7 may be present by default.
    features & (MULTI_WRITER_FORMAT_BITS & !sb::FEATURE_INCOMPAT_KV_DURABLE_TERM) == 0
}

/// Read the durable upgrade-intent marker off volume 0 (probe open —
/// nothing written, no guard taken).
async fn read_marker(vol0: &Path) -> Option<Vec<u8>> {
    let be = open_volume_probe(&vol0.display().to_string())
        .await
        .expect("probe open volume 0");
    be.getxattr(1, MW_UPGRADE_MARKER_XATTR)
        .await
        .expect("marker read")
}

// ---------------------------------------------------------------------------
// Phase A: `format --multi-writer` stamps all nine; default stamps none.
// ---------------------------------------------------------------------------

/// §6.2 pt 1 Phase A — the dark opt-in: the mw arm stamps all NINE bits in
/// one plan/one superblock write, and the default format's feature word is
/// byte-for-byte today's posture (the Phase-B flip is rung 10b's act).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_multi_writer_stamps_all_nine_and_default_stays_todays_posture() {
    let dir = tempfile::tempdir().unwrap();

    let default_vol = make_file(dir.path(), "default.meta");
    format_v3(&default_vol, VOL_LEN, &opts())
        .await
        .expect("default format");
    let feat = features_of(&default_vol).await;
    assert!(
        has_none_beyond_default(feat),
        "Phase-A pin: the DEFAULT format must stamp none of the mw bits \
         beyond today's posture (got {feat:#x})"
    );
    assert_ne!(
        feat & sb::FEATURE_INCOMPAT_KV_DURABLE_TERM,
        0,
        "bit 7 is today's fresh-format default and must stay"
    );

    let mw_vol = make_file(dir.path(), "mw.meta");
    format_v3_multi_writer(&mw_vol, VOL_LEN, &opts())
        .await
        .expect("mw format");
    let feat = features_of(&mw_vol).await;
    assert!(
        has_all_nine(feat),
        "--multi-writer must stamp all nine bits (got {feat:#x})"
    );

    // The nine are exactly bits 7..=15 — the §6.1 set, pinned as a mask so
    // a renumbering is a red test rather than silent aliasing.
    assert_eq!(
        MULTI_WRITER_FORMAT_BITS,
        (1u64 << 7)
            | (1 << 8)
            | (1 << 9)
            | (1 << 10)
            | (1 << 11)
            | (1 << 12)
            | (1 << 13)
            | (1 << 14)
            | (1 << 15),
        "the KD-MW-1 bit set is bits 7..=15"
    );
}

// ---------------------------------------------------------------------------
// The enable verb: ordered, idempotent, marker-bracketed.
// ---------------------------------------------------------------------------

/// The happy path over a 2-volume set: every volume ends with all nine
/// bits, the marker is gone, the set mounts writable, and a re-run is a
/// counted no-op (idempotent per-bit).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enable_multi_writer_stamps_ordered_idempotent_and_deletes_the_marker() {
    let dir = tempfile::tempdir().unwrap();
    let metas = [
        make_file(dir.path(), "m0.meta"),
        make_file(dir.path(), "m1.meta"),
    ];
    format_default_set(&metas).await;
    let paths = uris(&metas);

    let report = enable_multi_writer(&paths).await.expect("enable");
    assert!(
        report.bits_stamped > 0,
        "a default-posture set has bits to stamp"
    );
    for m in &metas {
        assert!(
            has_all_nine(features_of(m).await),
            "every member carries all nine after the verb"
        );
    }
    assert!(
        read_marker(&metas[0]).await.is_none(),
        "the verb's LAST act deletes the marker"
    );

    // Writable mount gate admits the upgraded set.
    let routed = open_routed_meta_set(&paths).await.expect("writable mount");
    let ino = routed
        .create(1, "post_upgrade", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("create on the upgraded set")
        .ino;
    assert!(ino >= 2);
    for vol in &routed.volumes {
        vol.shutdown().await.expect("clean shutdown");
    }

    // Idempotent re-run: nothing left to stamp, no marker minted.
    let rerun = enable_multi_writer(&paths).await.expect("re-run");
    assert_eq!(
        rerun.bits_stamped, 0,
        "re-running an upgraded set is a no-op"
    );
    assert!(read_marker(&metas[0]).await.is_none());
}

// ---------------------------------------------------------------------------
// MW-S1 — crash BETWEEN volumes (mixed bit-11 presence + marker).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mw_s1_crash_between_volumes_refuses_writable_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let metas = [
        make_file(dir.path(), "m0.meta"),
        make_file(dir.path(), "m1.meta"),
    ];
    format_default_set(&metas).await;
    let paths = uris(&metas);

    // Kill after volume 0's terminal bit (bit 11), before volume 1's first.
    let hooks = EnableMwHooks {
        crash_after: Some(EnableMwCrash::AfterBit {
            volume: 0,
            bits_done: 9,
        }),
    };
    let err = enable_multi_writer_with(&paths, &hooks)
        .await
        .expect_err("injected crash surfaces");
    assert!(
        err.to_string().contains("crash injection"),
        "the seam's own error: {err}"
    );

    // State on media: volume 0 fully stamped, volume 1 untouched, marker up.
    assert!(has_all_nine(features_of(&metas[0]).await));
    assert!(has_none_beyond_default(features_of(&metas[1]).await));
    assert!(
        read_marker(&metas[0]).await.is_some(),
        "marker survives the crash"
    );

    // A writable mount refuses, naming the lagging volume + the remedy.
    let refused = open_routed_meta_set(&paths).await;
    let msg = match refused {
        Ok(routed) => {
            for vol in &routed.volumes {
                let _ = vol.shutdown().await;
            }
            panic!("a mixed-stamp set under a marker must refuse writable mounts")
        }
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("enable-multi-writer"),
        "the refusal names the resume remedy: {msg}"
    );

    // Resume: re-running converges (prefix re-runs as no-ops), marker gone.
    let report = enable_multi_writer(&paths).await.expect("resume");
    assert!(report.bits_stamped > 0, "volume 1's bits still had to land");
    assert!(has_all_nine(features_of(&metas[1]).await));
    assert!(read_marker(&metas[0]).await.is_none());
    let routed = open_routed_meta_set(&paths)
        .await
        .expect("mount after resume");
    for vol in &routed.volumes {
        vol.shutdown().await.expect("clean shutdown");
    }
}

// ---------------------------------------------------------------------------
// MW-S1b — crash between EVERY adjacent bit pair (8 windows) + the
// marker-alone window (kill before any bit). Deterministic via the seam.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mw_s1b_kill_between_every_adjacent_bit_pair_refuses_then_resumes() {
    for bits_done in 0..=8usize {
        let dir = tempfile::tempdir().unwrap();
        let meta = make_file(dir.path(), "m0.meta");
        format_default_set(std::slice::from_ref(&meta)).await;
        let paths = uris(std::slice::from_ref(&meta));

        let hooks = EnableMwHooks {
            crash_after: Some(if bits_done == 0 {
                // MW-S1's "kill before any bit": marker alone.
                EnableMwCrash::AfterMarker
            } else {
                EnableMwCrash::AfterBit {
                    volume: 0,
                    bits_done,
                }
            }),
        };
        enable_multi_writer_with(&paths, &hooks)
            .await
            .expect_err("injected crash surfaces");

        // The volume carries a proper PREFIX of the order; bit 11 is
        // terminal by construction so it is absent in every window.
        let feat = features_of(&meta).await;
        assert_eq!(
            feat & sb::FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
            0,
            "window {bits_done}: bit 11 is TERMINAL — never present mid-sequence"
        );
        for (i, bit) in ENABLE_ORDER.iter().enumerate() {
            let present = feat & (1u64 << bit) != 0;
            // Bit 7 is a fresh-format default, so it reads present in
            // every window regardless of the verb's own progress.
            if *bit == 7 {
                assert!(present, "bit 7 rides the fresh-format default");
                continue;
            }
            assert_eq!(
                present,
                i < bits_done,
                "window {bits_done}: bit {bit} presence must match the stamp prefix"
            );
        }
        assert!(
            read_marker(&meta).await.is_some(),
            "window {bits_done}: the marker precedes any bit write and survives"
        );

        // The marker refusal covers the window regardless of bit state.
        let refused = open_routed_meta_set(&paths).await;
        let msg = match refused {
            Ok(routed) => {
                for vol in &routed.volumes {
                    let _ = vol.shutdown().await;
                }
                panic!("window {bits_done}: a marker must refuse writable mounts")
            }
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("enable-multi-writer"),
            "window {bits_done}: the refusal names the resume remedy: {msg}"
        );

        // Resume re-runs the prefix as no-ops and continues to the end.
        enable_multi_writer(&paths).await.expect("resume");
        assert!(has_all_nine(features_of(&meta).await));
        assert!(read_marker(&meta).await.is_none());
        let routed = open_routed_meta_set(&paths)
            .await
            .expect("mount after resume");
        for vol in &routed.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }
}

// ---------------------------------------------------------------------------
// MW-S2 — bit set, structure unminted: the first writable mount's minting
// acts (bit-9 ledger root, bit-8 partition adoption, bit-10 rebind) are the
// EXISTING crash-safe code paths; pin that they fire on a stamped mount.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mw_s2_first_writable_mount_minting_acts_fire_and_are_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "m0.meta");
    format_default_set(std::slice::from_ref(&meta)).await;
    let paths = uris(std::slice::from_ref(&meta));
    enable_multi_writer(&paths).await.expect("enable");

    // First writable mount after the stamp: bit 9's ledger root mints and
    // accounting engages; bit 8's partition read adopts the pre-partition
    // records; bit 10's staging scope is armed (no staging dirs here — the
    // meta-side arm is the open itself succeeding under the bit).
    let routed = open_routed_meta_set(&paths)
        .await
        .expect("first stamped mount");
    assert!(
        routed.volumes[0].block_refs_engaged(),
        "bit 9: the stamp engages durable block accounting on the next mount"
    );
    let ino = routed
        .create(1, "minted", libc::S_IFREG | 0o644, 1000, 1000)
        .await
        .expect("create under the stamped mount")
        .ino;
    routed
        .setxattr(ino, "user.tag", b"minted")
        .await
        .expect("xattr under the stamped mount");
    for vol in &routed.volumes {
        vol.shutdown().await.expect("clean shutdown");
    }

    // MW-S2's law: the minting acts are idempotent across a remount — the
    // second mount adopts the minted structures and the data is intact.
    let routed = open_routed_meta_set(&paths)
        .await
        .expect("second stamped mount");
    assert!(routed.volumes[0].block_refs_engaged());
    let got = routed.lookup(1, "minted").await.expect("lookup");
    assert_eq!(got.ino, ino, "ino stable across the stamped remount");
    let tag = routed
        .getxattr(ino, "user.tag")
        .await
        .expect("getxattr")
        .expect("present");
    assert_eq!(tag, b"minted");
    for vol in &routed.volumes {
        vol.shutdown().await.expect("clean shutdown");
    }
}

// ---------------------------------------------------------------------------
// MW-S3 — old binary meets a stamped volume: refuses loud via
// FEATURES_INCOMPAT_KNOWN (the existing law, pinned for the nine).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mw_s3_all_nine_bits_are_known_and_unknown_bits_refuse() {
    // Every one of the nine is inside THIS binary's known mask (so a
    // stamped volume mounts here) and each is a single bit (no aliasing).
    for bit in ENABLE_ORDER {
        let mask = 1u64 << bit;
        assert_eq!(
            FEATURES_INCOMPAT_KNOWN & mask,
            mask,
            "bit {bit} must be known to this binary"
        );
        assert_eq!(
            MULTI_WRITER_FORMAT_BITS & mask,
            mask,
            "bit {bit} must be in the one-act stamp set"
        );
    }

    // The refusal mechanism an OLD binary applies to these bits is the
    // same one THIS binary applies to bits above its own mask: pin it.
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "m0.meta");
    format_v3_multi_writer(&meta, VOL_LEN, &opts())
        .await
        .expect("mw format");
    let VolumeFormat::V3(mut sbv) = classify_volume(&meta).await.expect("classify") else {
        panic!("v3 expected");
    };
    sbv.features_incompat |= 1 << 40; // a bit outside FEATURES_INCOMPAT_KNOWN
    sb::write_superblock_v3(&meta, &sbv).await.expect("rewrite");
    let err = classify_volume(&meta)
        .await
        .expect_err("an unknown incompat bit refuses classification loud");
    assert!(
        err.to_string().contains("incompat"),
        "the refusal names the feature gate: {err}"
    );
}

// ---------------------------------------------------------------------------
// Shape (c) — legitimately partial populations must NOT trip the gate.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shape_c_partial_populations_never_trip_the_gate() {
    // Each row: a field-legal partial stamp that must keep mounting
    // writable exactly as today (subset NOT including bit 11, no marker).
    for (label, bits) in [
        ("standalone bit 15 (Phase-8 layout versions)", vec![15u64]),
        ("bit-5 runtime layout-deltas stamp", vec![5]),
        ("standalone bit 10 (writer-scoped staging)", vec![10]),
        ("bits 7+13 (durable term + incarnation keys)", vec![13]),
        ("bit 14 (claim set)", vec![14]),
        ("everything BUT bit 11", vec![9, 15, 12, 13, 8, 10, 14]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let meta = make_file(dir.path(), "m0.meta");
        format_default_set(std::slice::from_ref(&meta)).await;
        for bit in bits {
            match bit {
                5 => sb::set_layout_deltas_bit(&meta).await.map(|_| ()),
                8 => sb::set_partitioned_append_bit(&meta).await.map(|_| ()),
                9 => sb::set_block_refcounts_bit(&meta).await.map(|_| ()),
                10 => sb::set_writer_scoped_staging_bit(&meta).await.map(|_| ()),
                12 => sb::set_ino_lanes_bit(&meta).await.map(|_| ()),
                13 => sb::set_block_key_incarnation_bit(&meta).await.map(|_| ()),
                14 => sb::set_claim_set_bit(&meta).await.map(|_| ()),
                15 => sb::set_layout_versions_bit(&meta).await.map(|_| ()),
                other => panic!("unplanned bit {other}"),
            }
            .unwrap_or_else(|e| panic!("{label}: stamping bit {bit} failed: {e}"));
        }
        let paths = uris(std::slice::from_ref(&meta));
        let routed = open_routed_meta_set(&paths)
            .await
            .unwrap_or_else(|e| panic!("{label}: must mount writable as today, got: {e}"));
        for vol in &routed.volumes {
            vol.shutdown().await.expect("clean shutdown");
        }
    }
}

/// The foreign-tool tripwire: bit 11 WITHOUT the other eight refuses
/// writable, naming fsck (this state cannot be produced by the verb —
/// bit 11 is terminal by construction).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_bit_11_without_the_other_eight_refuses_naming_fsck() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "m0.meta");
    format_default_set(std::slice::from_ref(&meta)).await;
    sb::set_multi_writer_data_bit(&meta)
        .await
        .expect("stamp 11");
    let paths = uris(std::slice::from_ref(&meta));
    let err = match open_routed_meta_set(&paths).await {
        Ok(routed) => {
            for vol in &routed.volumes {
                let _ = vol.shutdown().await;
            }
            panic!("an orphan bit 11 must refuse writable mounts")
        }
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("fsck"),
        "the tripwire refusal names fsck: {err}"
    );
}

// ---------------------------------------------------------------------------
// The two §6.2 interaction rules, both directions.
// ---------------------------------------------------------------------------

/// `add-meta` onto a bit-11-uniform set stamps the new member to match as
/// part of the add (the fresh-format arm — no marker needed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_meta_onto_a_multi_writer_set_stamps_the_new_member_to_match() {
    let dir = tempfile::tempdir().unwrap();
    let metas = [make_file(dir.path(), "m0.meta")];
    format_mw_set(&metas).await;
    let paths = uris(&metas);

    let new_dev = make_file(dir.path(), "m1.meta");
    let taken = add_meta_volume(&paths, &new_dev.display().to_string(), &TakeSlots::Count(1))
        .await
        .expect("add-meta onto the mw set");
    assert!(!taken.is_empty(), "the add migrated slots");
    assert!(
        has_all_nine(features_of(&new_dev).await),
        "the new member is stamped to match — the uniformity law survives the add"
    );

    let mut extended = paths.clone();
    extended.push(new_dev.display().to_string());
    let routed = open_routed_meta_set(&extended)
        .await
        .expect("the grown set stays writable");
    for vol in &routed.volumes {
        vol.shutdown().await.expect("clean shutdown");
    }
}

/// The converse: a bit-11 volume refuses joining a non-upgraded set (read
/// at the add, BEFORE anything destructive).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_meta_of_a_bit_11_volume_into_a_non_upgraded_set_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let metas = [make_file(dir.path(), "m0.meta")];
    format_default_set(&metas).await;
    let paths = uris(&metas);

    // A foreign mw volume (its own single-member format).
    let foreign = make_file(dir.path(), "foreign.meta");
    format_v3_multi_writer(&foreign, VOL_LEN, &opts())
        .await
        .expect("foreign mw format");

    let err = add_meta_volume(&paths, &foreign.display().to_string(), &TakeSlots::Count(1))
        .await
        .expect_err("a bit-11 volume must refuse joining a non-upgraded set");
    assert!(
        err.to_string().contains("multi-writer"),
        "the refusal names the uniformity law: {err}"
    );
    // Nothing destructive happened: the set still mounts writable as today.
    let routed = open_routed_meta_set(&paths).await.expect("set untouched");
    for vol in &routed.volumes {
        vol.shutdown().await.expect("clean shutdown");
    }
}

// ---------------------------------------------------------------------------
// Serialization: the D0 guard, extended to the marker-tolerant resume open.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_enable_invocation_refuses_on_the_d0_guard() {
    let dir = tempfile::tempdir().unwrap();
    let meta = make_file(dir.path(), "m0.meta");
    format_default_set(std::slice::from_ref(&meta)).await;
    let paths = uris(std::slice::from_ref(&meta));

    // A concurrent holder of volume 0's D0 guard (what a live first
    // invocation holds across its whole run).
    let holder = KvMetaBackend::open(&meta).await.expect("guard holder");
    let err = enable_multi_writer(&paths)
        .await
        .expect_err("a second invocation refuses on the D0 guard");
    let msg = err.to_string();
    // The refusal happened BEFORE the verb's first write: no marker, no bit.
    assert!(has_none_beyond_default(features_of(&meta).await), "{msg}");
    holder.shutdown().await.expect("release");
    assert!(
        read_marker(&meta).await.is_none(),
        "no marker leaked: {msg}"
    );

    // Extended: the marker-tolerant RESUME open serializes the same way.
    let hooks = EnableMwHooks {
        crash_after: Some(EnableMwCrash::AfterMarker),
    };
    enable_multi_writer_with(&paths, &hooks)
        .await
        .expect_err("injected crash");
    assert!(
        read_marker(&meta).await.is_some(),
        "crashed state: marker up"
    );
    let holder = KvMetaBackend::open(&meta).await.expect("guard holder");
    enable_multi_writer(&paths)
        .await
        .expect_err("resume still refuses while the guard is held");
    assert!(
        read_marker(&meta).await.is_some(),
        "the refused resume wrote nothing"
    );
    holder.shutdown().await.expect("release");

    // With the guard free the resume converges.
    enable_multi_writer(&paths).await.expect("resume");
    assert!(has_all_nine(features_of(&meta).await));
    assert!(read_marker(&meta).await.is_none());
}
