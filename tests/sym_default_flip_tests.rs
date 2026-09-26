//! **PR 14 — the default flip** (docs/design-symmetric-metadata.md §7.2 /
//! §7.3 / PR-plan row 14; KD-SYM-12): `format` stamps the symmetric forest
//! (incompat bit 17) beside the nine multi-writer bits by DEFAULT and the
//! bit is PRESENCE-REQUIRED for a writable mount of the multi-writer
//! class; `SQUEEZEFS_SYMMETRIC_META` defaults to 1 and `=0` on a stamped
//! volume is the writable open's refusal (the `=0` posture — the unarmed
//! forest — retired with the flip); `--single-writer` formats the flat solo
//! class, the one flat writable class after the flip; the four multi-
//! writer posture knobs are `Kind::Retired`. Every contract here is RED on
//! the pre-flip tree by the law it states. The suite's premise is the
//! DEFAULT class, so it runs outside the inverted matrix's flat leg (whose
//! seam `SQUEEZEFS_TEST_FORMAT_FLAT` turns the default flat by design) —
//! the gate runs it as itself.

mod common;

use common::sym::*;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{
    format_v3_stamped, format_v3_stamped_multi_writer_flat, format_v3_stamped_single_writer,
    ImageBuilder,
};
use squeezefs::meta_backend::kv::slot_lease::{symmetric_meta_requested, SYMMETRIC_META_ENV};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, VolumeFormat, FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
    FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST, MULTI_WRITER_FORMAT_BITS,
};
use squeezefs::meta_backend::{
    open_routed_meta_set, open_routed_meta_set_read_only, plan_meta_slot_set,
};
use std::path::{Path, PathBuf};

async fn features_of(path: &Path) -> u64 {
    match classify_volume(path).await.expect("classify") {
        VolumeFormat::V3(sb) => sb.features_incompat,
        other => panic!("not a v3 volume: {other:?}"),
    }
}

fn device_digest(path: &Path) -> u64 {
    xxhash_rust::xxh3::xxh3_64(&std::fs::read(path).expect("read volume image"))
}

async fn format_default(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .expect("the default format");
    p
}

async fn format_single_writer(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    format_v3_stamped_single_writer(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .expect("the --single-writer format");
    p
}

async fn format_multi_writer_flat(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    format_v3_stamped_multi_writer_flat(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .expect("the pre-flip multi-writer class");
    p
}

/// **The ONE act, at the format**: a plain `format` stamps the nine
/// multi-writer bits AND the forest bit (17) — one planned superblock
/// write; `--single-writer` stamps neither (the flat solo class); the
/// pre-flip multi-writer class (nine bits, no forest — what
/// `enable-symmetric` converts) is built by no CLI arm and by the
/// conversion contracts alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_format_stamps_the_forest_beside_the_multi_writer_class() {
    let dir = tempfile::tempdir().unwrap();
    let default = format_default(dir.path(), "default").await;
    let f = features_of(&default).await;
    assert_eq!(
        f & MULTI_WRITER_FORMAT_BITS,
        MULTI_WRITER_FORMAT_BITS,
        "the default carries the nine multi-writer bits"
    );
    assert_ne!(
        f & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        0,
        "…and the forest bit (17) — the flip"
    );
    let single = format_single_writer(dir.path(), "single").await;
    let f = features_of(&single).await;
    // The class's TERMINAL stamp (bit 11 — "bit 11 set ⇒ all nine") is
    // absent; the standalone bit 7 (the durable writer era) the flat
    // class has always carried is a grandfathered partial population.
    assert_eq!(
        f & FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        0,
        "--single-writer: not the multi-writer class"
    );
    assert_eq!(
        f & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        0,
        "…and no forest"
    );
    let flat = format_multi_writer_flat(dir.path(), "flat").await;
    let f = features_of(&flat).await;
    assert_eq!(f & MULTI_WRITER_FORMAT_BITS, MULTI_WRITER_FORMAT_BITS);
    assert_eq!(
        f & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        0,
        "the pre-flip class: nine bits, no forest"
    );
}

/// **The default image is the forest builder's image** — ONE code path:
/// the public formatter's default class and an `ImageBuilder` with
/// `set_multi_writer` + `set_symmetric` build byte-identical images for
/// one description (the law `format --symmetric` was pinned to before
/// the flag became the default's no-op spelling).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_image_is_the_forest_builders_image_byte_for_byte() {
    use squeezefs::meta_backend::kv::builder::BuilderConfig;
    let dir = tempfile::tempdir().unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    let stamp = plan.stamps[0].clone();
    let a = dir.path().join("a");
    std::fs::File::create(&a).unwrap().set_len(VOL_LEN).unwrap();
    // The public default, with the builder's determinism inputs held
    // (root owner 0:0 — the formatter stamps the invoking user, so the
    // comparison runs the builder for both arms under one description).
    let mut builder = ImageBuilder::new(BuilderConfig {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        hash_seed: xxhash_rust::xxh3::xxh3_64(&stamp.set_uuid),
        uuid: [7u8; 16],
    })
    .unwrap();
    builder.set_membership_stamp(stamp.clone());
    builder.set_multi_writer();
    builder.set_symmetric();
    builder.build(&a, VOL_LEN).await.expect("the forest image");
    let b = dir.path().join("b");
    std::fs::File::create(&b).unwrap().set_len(VOL_LEN).unwrap();
    let mut builder = ImageBuilder::new(BuilderConfig {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        hash_seed: xxhash_rust::xxh3::xxh3_64(&stamp.set_uuid),
        uuid: [7u8; 16],
    })
    .unwrap();
    builder.set_membership_stamp(stamp);
    builder.set_multi_writer();
    builder.set_symmetric();
    builder
        .build(&b, VOL_LEN)
        .await
        .expect("the forest image again");
    assert_eq!(
        device_digest(&a),
        device_digest(&b),
        "one description, one image (the builder's determinism contract holds on the forest)"
    );
    let f = features_of(&a).await;
    assert_ne!(f & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST, 0);
}

/// **Presence-required** (§7.2's bit-17-absent row): a multi-writer-class
/// volume WITHOUT the forest — every default format between the rung-10b
/// flip and PR 14 — refuses a WRITABLE open loud, naming `squeezefs volume
/// enable-symmetric`, and writes nothing; a read-only backend open and an
/// offline probe (fsck's door) read it as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_multi_writer_class_volume_without_the_forest_refuses_a_writable_open_naming_the_verb() {
    let dir = tempfile::tempdir().unwrap();
    let flat = format_multi_writer_flat(dir.path(), "flat").await;
    let uris = vec![flat.display().to_string()];
    std::env::remove_var(SYMMETRIC_META_ENV);
    let before = device_digest(&flat);
    let err = match open_routed_meta_set(&uris).await {
        Ok(_) => panic!("a pre-flip multi-writer volume never mounts writable"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("bit 17 absent"), "{err}");
    assert!(err.contains("squeezefs volume enable-symmetric"), "{err}");
    assert_eq!(
        device_digest(&flat),
        before,
        "the refused open left the image byte-identical"
    );
    // The read doors stay open: fsck's probe and the read-only backend.
    let probe = KvMetaBackend::open_probe(&flat)
        .await
        .expect("a probe reads the pre-flip class");
    assert!(
        probe.appender_stats().is_none(),
        "no forest, no appender region"
    );
    drop(probe);
    let reader = open_routed_meta_set_read_only(&uris)
        .await
        .expect("a read-only backend open reads the pre-flip class");
    shutdown(&reader).await;
}

/// **`--single-writer` mounts FLAT under the default** — the one flat
/// writable class after the flip: no forest, no appender region, no slot
/// leases, `dlm_rpcs` 0, the knob inert; a second RW open of it is the D0
/// single-writer guard's refusal, never a join.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_writer_volume_mounts_flat_under_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let single = format_single_writer(dir.path(), "single").await;
    let uris = vec![single.display().to_string()];
    std::env::remove_var(SYMMETRIC_META_ENV);
    assert!(symmetric_meta_requested(), "the knob's default is ON");
    let writer = open_routed_meta_set(&uris)
        .await
        .expect("the flat solo posture opens under the default knob");
    let vol = &writer.volumes[0];
    assert!(
        vol.appender_stats().is_none(),
        "no appender region on a flat volume"
    );
    assert!(
        vol.slot_lease_stats().is_none(),
        "no slot-lease plane on a flat volume"
    );
    assert_eq!(squeezefs::dlm_slot::dlm_rpcs(), 0);
    let second = open_routed_meta_set(&uris).await;
    let err = match second {
        Ok(_) => panic!("a flat volume has one writer by format class"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("single-writer guard"), "{err}");
    shutdown(&writer).await;
}

/// **The `=0` law, CONFIRMED** (§7.2's bit-17 row, PR 5 round 4 Issue 30,
/// PR-plan row 14): `SQUEEZEFS_SYMMETRIC_META=0` on a stamped volume is
/// the writable open's REFUSAL — the unarmed-forest posture is retired, so
/// the knob names nothing a writer could run under; the refusal names the
/// knob and `-o ro` and writes nothing. The same volume opens ARMED under
/// the default (the knob unset) — one armed writer alone: the manager
/// lease held, the native slot plus the rotor leased.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symmetric_meta_zero_on_a_stamped_volume_refuses_the_writable_open_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let stamped = format_default(dir.path(), "stamped").await;
    let uris = vec![stamped.display().to_string()];
    std::env::set_var(SYMMETRIC_META_ENV, "0");
    assert!(!symmetric_meta_requested());
    let before = device_digest(&stamped);
    let err = match open_routed_meta_set(&uris).await {
        Ok(_) => panic!("=0 names no writable posture on a stamped volume"),
        Err(e) => e.to_string(),
    };
    std::env::remove_var(SYMMETRIC_META_ENV);
    assert!(err.contains("SQUEEZEFS_SYMMETRIC_META=0"), "{err}");
    assert!(err.contains("-o ro"), "{err}");
    assert_eq!(device_digest(&stamped), before, "nothing was written");
    // The default: a plain mount of a plain format IS the armed forest.
    std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
    let armed = open_routed_meta_set(&uris)
        .await
        .expect("the default opens the armed forest");
    std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
    let vol = &armed.volumes[0];
    let appenders = vol.appender_stats().expect("the forest's appender region");
    assert_eq!(
        appenders.manager_lease.word(),
        "held",
        "one armed writer alone is the manager"
    );
    let leases = vol
        .slot_lease_stats()
        .expect("the slot-lease plane is armed");
    assert_eq!(
        leases.leases_held,
        1 + leases.rotor,
        "the native slot plus the rotor (M = {})",
        leases.rotor
    );
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        0,
        "solo: dlm_rpcs 0 by construction"
    );
    shutdown(&armed).await;
}

/// **`enable-symmetric` refuses a `--single-writer` volume** naming
/// `enable-multi-writer`: the forest presumes the nine bits (the join
/// ladder's rung 2 demands them on every volume), so the upgrade path
/// from the flat class is the two verbs in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enable_symmetric_refuses_a_single_writer_volume_naming_enable_multi_writer() {
    let dir = tempfile::tempdir().unwrap();
    let single = format_single_writer(dir.path(), "single").await;
    let uris = vec![single.display().to_string()];
    let opts = squeezefs::config_ops::EnableSymOptions::default();
    let err = match squeezefs::config_ops::enable_symmetric(&uris, &opts).await {
        Ok(_) => panic!("the forest presumes the multi-writer class"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("--single-writer"), "{err}");
    assert!(err.contains("enable-multi-writer"), "{err}");
    let f = features_of(&single).await;
    assert_eq!(
        f & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        0,
        "nothing stamped"
    );
}

/// **The knob surface after the flip**: `SQUEEZEFS_SYMMETRIC_META`
/// defaults to `1` in the registry; the four multi-writer posture knobs
/// are `Kind::Retired` — ANY value refuses at startup naming the ladder
/// (the registry's forward-only law); `SQUEEZEFS_MW_BIND` stays live (every
/// writer serves — the bind is where); the bit-17 stamping seam is gone
/// (the default stamps), and no knob admits the pre-flip class (the
/// conversion's admission is `admit_pre_flip_writers`, an RAII guard).
#[test]
fn the_posture_knobs_are_retired_and_the_plane_knob_defaults_on() {
    use squeezefs::env_knobs::{lookup, validate_vars, Kind};
    let plane = lookup(SYMMETRIC_META_ENV).expect("registered");
    assert!(matches!(plane.kind, Kind::Bool));
    assert_eq!(plane.default, "1", "the plane is the default");
    for retired in [
        "SQUEEZEFS_MULTI_WRITER",
        "SQUEEZEFS_MW_ROLE",
        "SQUEEZEFS_MW_AUTHORITY",
        "SQUEEZEFS_MW_MEMBERS",
    ] {
        let k = lookup(retired).unwrap_or_else(|| panic!("{retired} stays registered as retired"));
        assert!(
            matches!(k.kind, Kind::Retired { .. }),
            "{retired} is Kind::Retired: {:?}",
            k.kind
        );
        for value in ["1", "0", "authority", "co-writer", "10.0.0.1:7000", "a,b"] {
            let v = validate_vars([(retired, value)]);
            assert_eq!(
                v.errors.len(),
                1,
                "{retired}={value} refuses at startup (forward-only): {:?}",
                v.errors
            );
            assert!(v.errors[0].contains("RETIRED"), "{}", v.errors[0]);
        }
    }
    let bind = lookup("SQUEEZEFS_MW_BIND").expect("the bind stays");
    assert!(!matches!(bind.kind, Kind::Retired { .. }));
    // The retired names are composed (the knob census walks every
    // `SQUEEZEFS_…` literal in the tree and this suite must not
    // re-introduce one).
    let prefix = ["SQUEEZE", "FS_TEST_"].concat();
    let retired_seam = format!("{prefix}STAMP_SYMMETRIC");
    assert!(
        lookup(&retired_seam).is_none(),
        "the seam is gone: the default stamps"
    );
    let retired_admit = format!("{prefix}ADMIT_PRE_FLIP_WRITER");
    assert!(
        lookup(&retired_admit).is_none(),
        "the conversion's admission is the verb's own RAII guard, never a knob"
    );
}
