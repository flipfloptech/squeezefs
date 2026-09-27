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
};
use squeezefs::meta_backend::kv::slot_lease::{symmetric_meta_requested, SYMMETRIC_META_ENV};
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, VolumeFormat, FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
    FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST, MULTI_WRITER_FORMAT_BITS,
};
use squeezefs::meta_backend::{
    open_routed_meta_set, open_routed_meta_set_read_only, plan_meta_slot_set, Metadata,
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

/// **The default class IS the explicit forest — ONE code path, pinned at
/// the PUBLIC-FORMATTER level** (PR 14 review fix round 1, Issue 9: the
/// first build compared two `ImageBuilder`s — a determinism tautology).
/// `format_v3_stamped` (the default class — `squeezefs format`'s arm) and
/// `format_v3_stamped_symmetric` (the explicit forest — `--symmetric`'s
/// spelling, the matrix's seam-blind builder) are RUN for one description
/// under one stamp: the decoded superblocks are equal once the per-format
/// volume uuid is normalised (the feature word with bit 17, the geometry,
/// the appender directory), the ledgers name the same tree roots, page 0
/// of the appender directory is the same `Free` page, and the §4.10
/// content digest is equal. Byte identity of the whole image is the
/// BUILDER's law (a public format mints its own uuid and node-seq base),
/// pinned where the builder is driven directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_class_is_the_explicit_forest_through_the_public_formatters() {
    use squeezefs::meta_backend::kv::appender::read_directory;
    use squeezefs::meta_backend::kv::builder::{digest_backend, format_v3_stamped_symmetric};
    use squeezefs::meta_backend::kv::checkpoint::read_newest_ledger;
    let dir = tempfile::tempdir().unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    let stamp = plan.stamps[0].clone();
    let a = dir.path().join("a");
    std::fs::File::create(&a).unwrap().set_len(VOL_LEN).unwrap();
    format_v3_stamped(&a, VOL_LEN, &set_opts(), stamp.clone())
        .await
        .expect("the default format");
    let b = dir.path().join("b");
    std::fs::File::create(&b).unwrap().set_len(VOL_LEN).unwrap();
    format_v3_stamped_symmetric(&b, VOL_LEN, &set_opts(), stamp)
        .await
        .expect("the explicit forest format");
    let (sb_a, sb_b) = match (
        classify_volume(&a).await.expect("classify a"),
        classify_volume(&b).await.expect("classify b"),
    ) {
        (VolumeFormat::V3(x), VolumeFormat::V3(y)) => (x, y),
        other => panic!("not two v3 volumes: {other:?}"),
    };
    assert_ne!(sb_a.uuid, sb_b.uuid, "the volume uuid is per format");
    let mut normalised = sb_b.clone();
    normalised.uuid = sb_a.uuid;
    assert_eq!(
        sb_a, normalised,
        "the superblocks agree in everything but the per-format uuid"
    );
    assert_ne!(
        sb_a.features_incompat & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        0
    );
    assert!(sb_a.appender_dir.len > 0, "the forest's appender directory");
    // The same tree roots (ids, in order) in the bootstrap ledger.
    let roots = |sb: &squeezefs::meta_backend::kv::superblock::SuperblockV3, p: &Path| {
        let start = sb.root_ledger.start;
        let p = p.to_path_buf();
        async move {
            read_newest_ledger(&p, start)
                .await
                .expect("ledger")
                .expect("a bootstrap record")
                .tree_roots
                .iter()
                .map(|r| r.tree_id)
                .collect::<Vec<u8>>()
        }
    };
    assert_eq!(roots(&sb_a, &a).await, roots(&sb_b, &b).await);
    // The same appender directory: page 0 `Free`, the same segment table.
    let page0 = |p: &Path, sb: &squeezefs::meta_backend::kv::superblock::SuperblockV3| {
        let p = p.to_path_buf();
        let sb = sb.clone();
        async move {
            read_directory(&p, &sb)
                .await
                .expect("directory")
                .into_iter()
                .find(|e| e.appender_id == 0)
                .and_then(|e| e.page)
                .map(|pg| (pg.state, pg.appender_id, pg.segments.clone(), pg.term))
        }
    };
    let (p0a, p0b) = (page0(&a, &sb_a).await, page0(&b, &sb_b).await);
    assert!(p0a.is_some(), "page 0 exists");
    assert_eq!(p0a, p0b, "the same Free page 0");
    // The same §4.10 content.
    let da = KvMetaBackend::open_probe(&a).await.expect("probe a");
    let db = KvMetaBackend::open_probe(&b).await.expect("probe b");
    assert_eq!(
        digest_backend(&da).await.expect("digest a"),
        digest_backend(&db).await.expect("digest b"),
        "one description, one content digest"
    );
}

/// **The production `format` ignores the matrix's flat seam** (PR 14
/// review fix round 1, Issue 7): `SQUEEZEFS_TEST_FORMAT_FLAT` turns a
/// DEFAULT-class request flat for the inverted matrix's flat leg — a
/// test's builder — and nothing pinned that the BINARY's `format`
/// (`squeezefs format`) does not read it. The CLI's default arm takes the
/// explicit forest class, so a plain `format` under the exported seam
/// stamps bit 17 exactly as it does without it; `--single-writer` stays
/// the one flat arm.
#[test]
fn the_binarys_format_stamps_the_forest_whatever_the_flat_seam_says() {
    use squeezefs::meta_backend::kv::builder::FORMAT_FLAT_SEAM;
    let dir = tempfile::tempdir().unwrap();
    let format_through_binary = |name: &str, seam: bool, single_writer: bool| -> u64 {
        let meta = dir.path().join(format!("{name}-meta"));
        let data = dir.path().join(format!("{name}-data"));
        std::fs::File::create(&meta)
            .unwrap()
            .set_len(256 * 1024 * 1024)
            .unwrap();
        std::fs::File::create(&data)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_squeezefs"));
        cmd.arg("format")
            .arg(format!("sqmeta://{}", meta.display()))
            .arg(format!("sqdata://{}", data.display()))
            .arg("--force");
        if single_writer {
            cmd.arg("--single-writer");
        }
        if seam {
            cmd.env(FORMAT_FLAT_SEAM, "1");
        } else {
            cmd.env_remove(FORMAT_FLAT_SEAM);
        }
        let out = cmd.output().expect("run squeezefs format");
        assert!(
            out.status.success(),
            "format {name} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(features_of(&meta))
    };
    let plain = format_through_binary("plain", false, false);
    let seamed = format_through_binary("seamed", true, false);
    assert_ne!(
        plain & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        0,
        "a plain format stamps the forest"
    );
    assert_ne!(
        seamed & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        0,
        "…and so does one under the exported seam — the binary never reads it"
    );
    assert_eq!(
        plain, seamed,
        "the feature word is the same with and without the seam"
    );
    let single = format_through_binary("single", true, true);
    assert_eq!(
        single & FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        0,
        "--single-writer is the one flat arm"
    );
}

/// **Presence-required** (§7.2's bit-17-absent row): a multi-writer-class
/// volume WITHOUT the forest — every default format between the rung-10b
/// flip and PR 14 — refuses a WRITABLE open loud, naming `squeezefs volume
/// enable-symmetric`, and writes nothing; a read-only backend open and an
/// offline probe (fsck's door) read it as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_multi_writer_class_volume_without_the_forest_refuses_a_writable_open_naming_the_verb() {
    // The process env is shared: every contract that sets or clears the
    // plane knob holds the seam lock (the `=0` contract's `set_var` raced
    // this one's `remove_var` — 2 of 3 runs red before the lock).
    let _g = SEAM.lock().await;
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
    let _g = SEAM.lock().await;
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
    let _g = SEAM.lock().await;
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

/// **`enable-symmetric` upgrades a `--single-writer` volume in ONE act**
/// (PR 14 fix round 3, class B — design-symmetric-metadata §7.2): the
/// forest presumes the nine multi-writer bits (the join ladder's rung 2
/// demands them on every volume), so the verb stamps them as its first
/// half and converts in the same invocation; `--dry-run` reports the
/// stamps it would write and plans the conversion, writing nothing.
/// `enable-multi-writer` — which left a set in the pre-flip class no
/// writer's door admits — is retired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enable_symmetric_upgrades_a_single_writer_volume_in_one_act() {
    let dir = tempfile::tempdir().unwrap();
    let single = format_single_writer(dir.path(), "single").await;
    let uris = vec![single.display().to_string()];
    let before = features_of(&single).await;
    let dry = squeezefs::config_ops::enable_symmetric(
        &uris,
        &squeezefs::config_ops::EnableSymOptions {
            dry_run: true,
            ..Default::default()
        },
    )
    .await
    .expect("the dry run plans a single-writer volume");
    assert_eq!(
        dry.multi_writer_bits_stamped,
        (MULTI_WRITER_FORMAT_BITS & !before).count_ones() as usize,
        "the dry run reports every nine-bit stamp the real run writes: {dry:?}"
    );
    assert_eq!(
        dry.rows.len(),
        1,
        "the dry run plans the conversion beside the stamp: {dry:?}"
    );
    assert_eq!(
        features_of(&single).await,
        before,
        "a dry run writes nothing"
    );
    let report = squeezefs::config_ops::enable_symmetric(
        &uris,
        &squeezefs::config_ops::EnableSymOptions::default(),
    )
    .await
    .expect("one act: the nine bits, then the forest");
    assert_eq!(
        report.multi_writer_bits_stamped,
        dry.multi_writer_bits_stamped
    );
    let f = features_of(&single).await;
    assert_eq!(
        f & MULTI_WRITER_FORMAT_BITS,
        MULTI_WRITER_FORMAT_BITS,
        "the nine multi-writer bits stamped"
    );
    assert_ne!(
        f & FEATURE_INCOMPAT_KV_SYMMETRIC_FOREST,
        0,
        "the forest stamped in the same act"
    );
    let armed = open_routed_meta_set(&uris)
        .await
        .expect("the upgraded volume mounts as the armed default");
    assert!(armed.volumes[0].slot_lease_armed());
    shutdown(&armed).await;
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

/// **An offline probe's inode population counts every slot's durable
/// cursor** (PR 14 review fix round 1 — the require-mount gate's `df` row,
/// a regression of the branch's own §4.4bd): since a forest's ledger stamp
/// carries no per-slot cursors, the open seeded the guest cells from OUR
/// regions' pages and leases alone — a probe holds none, so `squeezefs df`
/// (`live_inodes`, `statfs`'s `f_files`) read ONE inode on a volume whose
/// mount had created forty (they minted into the rotor's guest keyspaces,
/// the native watermark unmoved). Every `Live` page's checkpoint-time slot
/// entries and tree 0's word for every slot seed the cells as floors on
/// every posture now; the count is the §4.8 law's pessimistic progression.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_probes_inode_population_counts_the_rotors_guest_cursors() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let stamped = format_default(dir.path(), "df-probe").await;
    let uris = vec![stamped.display().to_string()];
    std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
    let writer = open_routed_meta_set(&uris)
        .await
        .expect("the default opens the armed forest");
    std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
    for i in 0..40 {
        writer
            .create(1, &format!("f{i:03}"), libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create");
    }
    let live_at_writer = writer.volumes[0].live_inodes();
    assert!(
        live_at_writer >= 40,
        "the writer counts its own mints: {live_at_writer}"
    );
    writer.volumes[0]
        .checkpoint_now()
        .await
        .expect("checkpoint");
    shutdown(&writer).await;
    drop(writer);
    let probe = KvMetaBackend::open_probe(&stamped)
        .await
        .expect("the offline probe");
    let live = probe.live_inodes();
    assert!(
        live >= 40,
        "a probe counts the rotor's guest cursors — forty creates, {live} counted"
    );
    assert!(
        live <= live_at_writer,
        "…and never above the writer's own progression ({live_at_writer}): {live}"
    );
}
