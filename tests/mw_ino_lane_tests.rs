//! **Per-writer ino lanes** — pre-RC engineering spec §6.2 **item 5**
//! (rulings **D8**/**D9**; design
//! `docs/design-mw-cursors-and-incarnation.md`), behind incompat bit 12,
//! built but **NOT stamped**.
//!
//! The assumption being broken: `next_ino` is a per-mount atomic over a
//! shared namespace (`kv/backend.rs`), so two writers mint **duplicate
//! inos**, which alias files immediately — and because the daemon's IPC
//! binding table rests on the monotonic never-reused ino law
//! (pre-RC spec §8, "daemon-owned binding table resting on the monotonic
//! never-reused ino law"), the alias reaches fd bindings too.
//!
//! What is pinned here, and what deliberately is NOT: these contracts prove
//! the ino namespace is **expressible for N appenders** — mints are
//! lane-disjoint, every ino is attributable to its minter, recovery is
//! lane-correct across a crash, and a lane on a volume whose format does
//! not express lanes is refused LOUD. Who MAY mint, and the partitioning
//! that keeps writers disjoint, is §6.9 **S4/S8** and is not built here.
//!
//! Three invariants had to survive intact, and each has its own case:
//! monotonic never-reused inos, the global-ino stability the frozen routing
//! width depends on, and `statfs`'s live-inode count (POSIX-1).

use squeezefs::lane_core::LaneCursor;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options};
use squeezefs::meta_backend::kv::ino_lane::{
    first_ino_in_lane, ino_lane_of, minted_in_ino_lane, recover_ino_floor, InoSpace, LOCAL_INO_BASE,
};
use squeezefs::meta_backend::kv::journal::AppendPartition;
use squeezefs::meta_backend::kv::slot_cursor_core::SlotCursor;
use squeezefs::meta_backend::kv::superblock::{
    classify_volume, set_ino_lanes_bit, VolumeFormat, FEATURES_INCOMPAT_KNOWN,
    FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION, FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
    FEATURE_INCOMPAT_KV_INO_LANES, FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
};
use squeezefs::meta_backend::{
    make_global_ino_width, route_ino_width, Metadata, DERIVED_ROUTING_WIDTH,
};
use std::collections::HashSet;
use tempfile::NamedTempFile;

const META_LEN: u64 = 256 * 1024 * 1024;

fn opts() -> FormatV3Options {
    FormatV3Options {
        node_size: 64 * 1024,
        journal_len_override: None,
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

fn part(writers: u16, writer: u16) -> AppendPartition {
    AppendPartition::new(writers, writer).expect("legal partition")
}

/// Format the SINGLE-WRITER (unstamped) class — the pre-flip default.
/// This suite's lane contracts exercise bit 12 IN ISOLATION, so the base
/// image must not carry the rest of the nine-bit set the rung-10b
/// Phase-B default now stamps.
async fn format_meta(path: &std::path::Path) {
    squeezefs::meta_backend::kv::builder::format_v3_single_writer(path, META_LEN, &opts())
        .await
        .expect("format v3 meta volume (single-writer class)");
}

/// [`format_meta`] plus the isolated bit-12 stamp (the upgrade-verb act,
/// applied alone).
async fn format_meta_laned(path: &std::path::Path) {
    format_meta(path).await;
    assert!(
        set_ino_lanes_bit(path).await.expect("stamp bit 12"),
        "a single-writer-class format must NOT already carry bit 12 — \
         stamping it here in isolation is this suite's whole point"
    );
}

// ===========================================================================
// 1. The lane law itself (pure — `crate::lane_core` + `kv::ino_lane`).
// ===========================================================================

/// Disjointness and attribution: four appenders minting concurrently from
/// their own lanes produce **no duplicate ino**, every ino names its
/// minter, and each appender's own sequence is strictly monotone. This is
/// the property that makes the partition arbitration-free — an appender
/// needs to know only its own id.
#[test]
fn lanes_are_disjoint_monotone_and_attributable() {
    const W: u16 = 4;
    const N: u64 = 1_000;
    let mut seen: HashSet<u64> = HashSet::new();
    for writer in 0..W {
        let p = part(W, writer);
        let cursor = LaneCursor::new(
            LOCAL_INO_BASE,
            u64::from(W),
            u64::from(writer),
            LOCAL_INO_BASE,
        );
        let mut prev = 0u64;
        for _ in 0..N {
            let ino = cursor.mint();
            assert!(
                ino > prev,
                "appender {writer}'s own sequence must be strictly monotone"
            );
            prev = ino;
            assert_eq!(
                ino_lane_of(ino, W),
                u64::from(writer),
                "ino {ino} must be attributable to appender {writer} — recovery and \
                 fsck classify a foreign writer's inos with exactly this function"
            );
            assert!(
                seen.insert(ino),
                "ino {ino} was minted by two appenders — duplicate inos alias files \
                 immediately (spec §6.2 item 5)"
            );
        }
        assert_eq!(
            first_ino_in_lane(p),
            LOCAL_INO_BASE + u64::from(writer),
            "lane {writer}'s first ino is base + writer"
        );
        assert_eq!(
            minted_in_ino_lane(cursor.snapshot(), cursor.start(), p),
            N,
            "the lane's exact mint count is what statfs needs"
        );
        assert_eq!(cursor.minted(), N, "the cursor counts its own mints");
    }
    assert_eq!(seen.len() as u64, N * u64::from(W), "no ino minted twice");
}

/// **Tie test** (the repo's drift-is-red pattern, not a comment): solo —
/// the shipped posture — is *arithmetically identical* to today's dense
/// cursors. A `LaneCursor` at `AppendPartition::SOLO` produces the same
/// sequence as the VL5b `SlotCursor` and as a plain `fetch_add(1)`
/// watermark, from any floor. If the lane arithmetic ever drifts from the
/// shipped counter, this fails.
#[test]
fn solo_lane_cursor_ties_the_shipped_cursor() {
    for floor in [2u64, 3, 17, 4096, 1 << 20] {
        let lane = LaneCursor::new(LOCAL_INO_BASE, 1, 0, floor);
        let slot = SlotCursor::new(floor);
        for dense in (floor.max(2)..).take(64) {
            let l = lane.mint();
            let s = slot.mint();
            assert_eq!(
                l, s,
                "solo lane must tie the VL5b SlotCursor at floor {floor}"
            );
            assert_eq!(l, dense, "solo lane must tie a dense fetch_add(1)");
        }
        assert_eq!(
            minted_in_ino_lane(lane.snapshot(), LOCAL_INO_BASE, AppendPartition::SOLO),
            lane.snapshot() - LOCAL_INO_BASE,
            "solo's mint count collapses to today's `cursor − 2` arithmetic"
        );
    }
}

/// Recovery: whatever dense floor a mount recovers (§4.8's
/// `max(ledger watermark, replayed + 1)`), rounding it into the lane lands
/// **strictly above every ino that lane had committed**, is idempotent, and
/// never regresses a fresher mint. The values the rounding skips are burned
/// — the same law §4.8 already states for a failed create's ino.
#[test]
fn recovery_rounds_into_the_lane_and_never_re_mints() {
    const W: u16 = 4;
    for writer in 0..W {
        let p = part(W, writer);
        let cursor = LaneCursor::new(
            LOCAL_INO_BASE,
            u64::from(W),
            u64::from(writer),
            LOCAL_INO_BASE,
        );
        let mut committed = Vec::new();
        for _ in 0..37 {
            committed.push(cursor.mint());
        }
        // The dense watermark a ledger/replay fold would produce.
        let dense = committed.iter().copied().max().unwrap() + 1;
        let floor = recover_ino_floor(dense, *committed.iter().max().unwrap(), p);
        assert_eq!(
            ino_lane_of(floor, W),
            u64::from(writer),
            "a recovered floor must be IN the appender's lane"
        );
        assert!(
            committed.iter().all(|c| floor > *c),
            "the recovered floor must dominate every committed ino of this lane"
        );
        assert_eq!(
            recover_ino_floor(floor, 0, p),
            floor,
            "rounding is idempotent — re-seeding never advances a cursor"
        );
        // Monotone install: a stale floor cannot pull the cursor back.
        let fresh = LaneCursor::new(LOCAL_INO_BASE, u64::from(W), u64::from(writer), floor);
        let ahead = fresh.mint();
        fresh.install_floor(LOCAL_INO_BASE);
        assert!(
            fresh.mint() > ahead,
            "install_floor must never regress a fresher mint"
        );
    }
}

/// Global-ino stability — the invariant the frozen routing width depends
/// on: laned locals are *sparse*, and sparseness must not disturb the
/// encoding. Every laned local of every appender still round-trips through
/// [`route_ino_width`] / [`make_global_ino_width`] and every global ino is
/// unique.
#[test]
fn laned_locals_keep_global_ino_stability() {
    const W: u16 = 4;
    let width = u64::from(DERIVED_ROUTING_WIDTH);
    let mut globals: HashSet<u64> = HashSet::new();
    for writer in 0..W {
        let cursor = LaneCursor::new(
            LOCAL_INO_BASE,
            u64::from(W),
            u64::from(writer),
            LOCAL_INO_BASE,
        );
        for slot in [0u64, 1, 63, 65_535] {
            for _ in 0..8 {
                let local = cursor.mint();
                let global = make_global_ino_width(local, slot, width);
                assert_eq!(
                    route_ino_width(global, width),
                    (slot, local),
                    "a laned local must route back EXACTLY (slot {slot}, local {local})"
                );
                assert!(
                    globals.insert(global),
                    "global ino {global} collided — the routing encoding must stay injective \
                     under laned (sparse) locals"
                );
            }
        }
    }
}

// ===========================================================================
// 2. Ruling D9's compatibility boundary + the bit-disjointness pin.
// ===========================================================================

/// **The bit ledger.** A sibling wave had two agents independently claim
/// bit 8, which is silent on-disk ALIASING, not a merge inconvenience — so
/// the four multi-writer format bits are pinned here: partitioned append 8,
/// durable block refcounts 9, ino lanes 12, block-key incarnation 13,
/// pairwise disjoint, all understood by this binary — stamped by the
/// DEFAULT `format` since the rung-10b Phase-B flip and by none of the
/// `--single-writer` class.
///
/// **Bits 12/13, not 10/11** — the same collision, caught a second time:
/// bits **10** (writer-scoped staging keys, §6.2 item 8) and **11** (the S7
/// data-plane fence) were claimed by branches authored in parallel with
/// this one, so this wave moved up rather than alias them. The reservation
/// is asserted below as a MASK, not as a pair of constants this tree does
/// not yet contain: whichever branch lands first, the wave's two bits must
/// stay outside `1 << 10 | 1 << 11` forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_multi_writer_format_bits_are_disjoint_and_class_scoped() {
    assert_eq!(FEATURE_INCOMPAT_KV_PARTITIONED_APPEND, 1 << 8);
    assert_eq!(FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS, 1 << 9);
    assert_eq!(
        FEATURE_INCOMPAT_KV_INO_LANES,
        1 << 12,
        "the §6.2 item-5 bit is 12 (8 = partitioned append, 9 = durable block \
         refs, 10 = writer-scoped staging, 11 = the S7 data-plane fence)"
    );
    assert_eq!(
        FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION,
        1 << 13,
        "the §6.2 item-6 bit is 13"
    );
    // The parallel-branch reservation, as a mask: bits 10 and 11 belong to
    // writer-scoped staging and the S7 data-plane fence. Their constants
    // live on other branches, so aliasing them can only be prevented from
    // here — and a mask assertion survives their landing, where an
    // enumerated equality would not.
    const RESERVED_PARALLEL: u64 = (1 << 10) | (1 << 11);
    assert_eq!(
        (FEATURE_INCOMPAT_KV_INO_LANES | FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION)
            & RESERVED_PARALLEL,
        0,
        "this wave must not claim bit 10 (writer-scoped staging) or bit 11 \
         (S7 data-plane fence) — two definitions of one bit is silent on-disk \
         aliasing, not a merge inconvenience"
    );
    let bits = [
        FEATURE_INCOMPAT_KV_PARTITIONED_APPEND,
        FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
        FEATURE_INCOMPAT_KV_INO_LANES,
        FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION,
    ];
    for (i, a) in bits.iter().enumerate() {
        assert_ne!(
            FEATURES_INCOMPAT_KNOWN & a,
            0,
            "this binary must understand every multi-writer bit"
        );
        for b in bits.iter().skip(i + 1) {
            assert_eq!(a & b, 0, "two multi-writer format bits must be disjoint");
        }
    }

    // …and `format` stamps neither of the two this wave adds (ruling D9).
    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path()).await;
    let VolumeFormat::V3(sb) = classify_volume(meta.path()).await.unwrap() else {
        panic!("expected v3");
    };
    assert_eq!(
        sb.features_incompat
            & (FEATURE_INCOMPAT_KV_INO_LANES | FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION),
        0,
        "a --single-writer format must stamp neither bit 12 nor bit 13 — the \
         upgrade verb (or the stamped default class) owns that act"
    );

    // …and the DEFAULT format stamps BOTH, as part of the one-act nine-bit
    // set (rung 10b — the Phase-B flip).
    let mw = NamedTempFile::new().unwrap();
    format_v3(mw.path(), META_LEN, &opts())
        .await
        .expect("default format");
    let VolumeFormat::V3(sb) = classify_volume(mw.path()).await.unwrap() else {
        panic!("expected v3");
    };
    assert_eq!(
        sb.features_incompat
            & (FEATURE_INCOMPAT_KV_INO_LANES | FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION),
        FEATURE_INCOMPAT_KV_INO_LANES | FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION,
        "the default format stamps bits 12 and 13 (the Phase-B flip)"
    );
}

/// An un-stamped volume behaves EXACTLY as today: solo mints are dense from
/// 2, a non-solo lane is REFUSED loud, and mounting (plus a real commit)
/// leaves sector 0 byte-identical.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unstamped_volume_mints_dense_and_refuses_a_lane() {
    let meta = NamedTempFile::new().unwrap();
    format_meta(meta.path()).await;
    let before = std::fs::read(meta.path()).unwrap()[..4096].to_vec();

    let kv = KvMetaBackend::open(meta.path()).await.expect("mount");
    assert!(!kv.ino_lanes_stamped(), "an un-stamped volume has no lanes");

    // Dense minting, unchanged: consecutive inos from the watermark.
    let a = kv.allocate_ino();
    let b = kv.allocate_ino();
    assert_eq!(b, a + 1, "solo minting stays DENSE on an un-stamped volume");
    assert_eq!(
        kv.allocate_ino_in(AppendPartition::SOLO).unwrap(),
        b + 1,
        "the solo lane IS the shipped mint path"
    );

    // A real commit, so the byte-identity claim covers a publish and not
    // just an idle mount.
    kv.create(1, "unstamped", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create");

    let err = kv
        .allocate_ino_in(part(4, 1))
        .expect_err("a lane on an un-stamped volume must be refused");
    let msg = format!("{err}");
    assert!(
        msg.contains("bit 12") && msg.contains("lane"),
        "the refusal must name the missing format bit, not fail obscurely: {msg}"
    );

    kv.shutdown().await.expect("clean shutdown");
    let after = std::fs::read(meta.path()).unwrap()[..4096].to_vec();
    assert_eq!(
        before, after,
        "mounting AND committing on an un-stamped volume must leave sector 0 \
         byte-identical (ruling D9)"
    );
}

// ===========================================================================
// 3. A stamped volume: real mints, real recovery, real statfs.
// ===========================================================================

/// The whole point, on a real mount: two appenders minting from ONE volume
/// produce disjoint inos, and neither collides with the dense watermark the
/// solo path uses. The ledger watermark the mount publishes DOMINATES every
/// lane, which is what makes a successor's recovery safe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn appenders_on_one_volume_mint_disjoint_inos() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_laned(meta.path()).await;
    let kv = KvMetaBackend::open(meta.path()).await.expect("mount");
    assert!(kv.ino_lanes_stamped(), "the stamp engages lanes");

    let mut seen: HashSet<u64> = HashSet::new();
    // The solo/legacy path stays live on the same volume (a lane-aware
    // mount must not break the dense watermark it recovered from).
    for _ in 0..8 {
        assert!(seen.insert(kv.allocate_ino()));
    }
    for writer in 0..4u16 {
        let p = part(4, writer);
        for _ in 0..16 {
            let ino = kv.allocate_ino_in(p).expect("lane mint");
            assert_eq!(ino_lane_of(ino, 4), u64::from(writer));
            assert!(
                seen.insert(ino),
                "ino {ino} minted twice across appenders/watermark"
            );
        }
    }
    // Guest spaces are laned too: slot migration can leave two writers
    // believing they host one slot (a membership disagreement), and lanes
    // make that survivable instead of aliasing. Each slot is its own
    // keyspace, so uniqueness is checked per slot.
    for slot in [0u16, 7] {
        let mut per_slot: HashSet<u64> = HashSet::new();
        for writer in 0..4u16 {
            let p = part(4, writer);
            for _ in 0..8 {
                let raw = kv.allocate_guest_ino_in(slot, p).expect("guest lane mint");
                assert_eq!(ino_lane_of(raw, 4), u64::from(writer));
                assert!(
                    per_slot.insert(raw),
                    "guest ino {raw} minted twice in slot {slot} across appenders"
                );
            }
        }
    }

    let watermark = kv.next_ino();
    for writer in 0..4u16 {
        let snap = kv
            .ino_lane_snapshot(InoSpace::Native, part(4, writer))
            .expect("lane exists");
        assert!(
            watermark >= snap,
            "the published watermark ({watermark}) must dominate lane {writer}'s cursor \
             ({snap}) — a successor recovers from it and rounds up into its own lane"
        );
    }
    kv.shutdown().await.expect("clean shutdown");
}

/// POSIX-1 (`statfs` `f_ffree`): a lane cursor STRIDES, so counting it
/// densely would over-report `IUsed` by the appender count — the same class
/// of error that made `df -i` under-report by ~64× before the MINT_SPREAD
/// fix, one level down. The live count must be exact for any mix of dense,
/// lane and guest mints, minus destroys.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_inode_count_is_exact_under_lanes() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_laned(meta.path()).await;
    let kv = KvMetaBackend::open(meta.path()).await.expect("mount");

    let base = kv.live_inodes();
    let mut minted = 0u64;
    for _ in 0..5 {
        kv.allocate_ino();
        minted += 1;
    }
    for writer in 0..4u16 {
        for _ in 0..7 {
            kv.allocate_ino_in(part(4, writer)).expect("lane mint");
            minted += 1;
        }
        for _ in 0..3 {
            kv.allocate_guest_ino_in(9, part(4, writer))
                .expect("guest lane mint");
            minted += 1;
        }
    }
    assert_eq!(
        kv.live_inodes(),
        base + minted,
        "every mint counts EXACTLY once — a strided cursor's progression is \
         (cursor − first) / writers, never (cursor − 2)"
    );
    kv.shutdown().await.expect("clean shutdown");
}

/// **The crash leg.** A lane's inos must survive a crash + remount with
/// the never-reused law intact: the recovered dense watermark (§4.8's
/// `max(ledger, replayed + 1)` — here fed by the DUR-8c replay fold, since
/// the crash skips the final checkpoint) rounds up into the lane strictly
/// above every ino the lane committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_and_remount_never_re_mints_a_lane_ino() {
    let meta = NamedTempFile::new().unwrap();
    format_meta_laned(meta.path()).await;

    let p = part(4, 2);
    let committed = {
        let kv = KvMetaBackend::open(meta.path()).await.expect("mount");
        let mut committed = Vec::new();
        for _ in 0..6 {
            let ino = kv.allocate_ino_in(p).expect("lane mint");
            // A record that MENTIONS the ino — the xattr key carries it, so
            // the replay fold raises the watermark even though no inode
            // record was created (DUR-8c).
            kv.setxattr(ino, "user.lane", b"1")
                .await
                .expect("commit a record naming the laned ino");
            committed.push(ino);
        }
        // Crash: drop WITHOUT shutdown — no final checkpoint, so recovery
        // must come from the journal replay window.
        drop(kv);
        committed
    };

    let kv = KvMetaBackend::open(meta.path()).await.expect("remount");
    let floor = recover_ino_floor(kv.next_ino(), 0, p);
    assert!(
        committed.iter().all(|c| floor > *c),
        "the recovered lane floor {floor} must dominate every committed ino {committed:?} \
         — a re-minted ino aliases a file that survived the crash"
    );
    kv.install_ino_lane_floor(InoSpace::Native, p, kv.next_ino())
        .expect("install the recovered floor");
    let next = kv.allocate_ino_in(p).expect("post-crash lane mint");
    assert!(
        committed.iter().all(|c| next > *c),
        "the first post-crash mint {next} must be above every pre-crash ino"
    );
    assert_eq!(
        ino_lane_of(next, 4),
        2,
        "the post-crash mint must stay in appender 2's lane"
    );
    kv.shutdown().await.expect("clean shutdown");
}
