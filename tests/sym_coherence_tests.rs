//! Symmetric metadata program, PR 5 — **GPFS-strict read tokens, the bset
//! frame v2 screen, and recall-driven free-grace**
//! (`docs/design-symmetric-metadata.md` §5.7 (R-SYM-4, KD-SYM-19), §5.8.2,
//! §5.8.6, §11 the Token family, §8 gate 5).
//!
//! Part A — **the frame screen** (§5.8.2): under bit 17 every bset frame is
//! v2, `magic ‖ version=2 ‖ reserved ‖ node_seq ‖ padded_len ‖ bset_len ‖
//! appender_id ‖ g ‖ checksum` (40 B), `g` the SLOT's lease generation at
//! the write. A loader with the slot's `g_current` and the tail the last
//! release recorded for the leaf treats a frame as FOREIGN iff (1) `g >
//! g_current`, (2) `g` ≤ the recorded generation AND the frame sits at or
//! past the recorded tail, (3) `g` is lower than an earlier frame's in the
//! same log — and keeps a predecessor's frame BEFORE the recorded tail.
//! Frame v1 stays byte-identical on a bit-17-absent volume.
//!
//! Part B — **tokens** (§5.7): a `-o ro` reader of an armed symmetric
//! volume serves every user-visible object under a READ TOKEN from the
//! object's holder, whose grant carries the records; the holder recalls
//! every reader's token BEFORE a conflicting commit lands; a reader acks a
//! recall only after its in-flight serves drain and its block-key census is
//! purged; a foreign create is visible at the reader's NEXT resolve —
//! exact, never bounded (`reader_staleness_bound_ms == 0`).
//!
//! Part C — **recall-driven free-grace** (§5.7.3): a terminal free of a
//! block whose freeing publish recalled every token bypasses the grace ring
//! (`free_grace_recall_gated_frees`); a free issued while a LIVE member's
//! recall is unacked rides the ring (`free_grace_timeout_deferrals`), the
//! ring's surviving role.

use squeezefs::meta_backend::kv::node::{
    append_bset, encode_bset_frame, load_node_screened, write_node, AppendDest, FrameScreen,
    FrameStamp, NodeLayout, NodeWriteParams, BSET_FRAME_LEN, BSET_FRAME_V2_LEN, MIN_NODE_SIZE,
    NODE_PAGE,
};
use squeezefs::meta_backend::kv::record::{inode_key, InodeValue, Record, TREE_INODES};
use squeezefs::meta_backend::kv::{
    KvError, META_KV_APPENDER_FENCE_BREACH, META_KV_FOREIGN_FRAMES_SCREENED,
    META_KV_FOREIGN_FRAME_OVERWRITE_DETECTED,
};
use std::sync::atomic::Ordering;
use tempfile::NamedTempFile;

// ===========================================================================
// Part A — the frame screen (§5.8.2)
// ===========================================================================

const NODE_SIZE: usize = MIN_NODE_SIZE;
const VOL_SIZE: u64 = 4 * 1024 * 1024;
/// The extent the forged log lives in.
const ADDR: u64 = NODE_SIZE as u64;
const NODE_SEQ: u64 = 0x5EED;

/// The screen's counters are process-global; every pin reading a delta
/// serializes on this (the suites run under `--test-threads=1` in the
/// gate, but a parallel run must not read another pin's delta).
static COUNTERS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn fresh_volume() -> NamedTempFile {
    let tmp = NamedTempFile::new().expect("create temp volume");
    tmp.as_file().set_len(VOL_SIZE).expect("size volume");
    tmp
}

fn stamp(appender_id: u32, g: u32) -> FrameStamp {
    FrameStamp { appender_id, g }
}

fn v2() -> NodeLayout {
    NodeLayout::new_symmetric(NODE_SIZE).expect("symmetric layout")
}

fn put(ino: u64, seq: u64) -> Record {
    let v = InodeValue {
        mode: 0o100644,
        uid: seq as u32,
        gid: 0,
        nlink: 1,
        flags: 0,
        rdev: 0,
        size: seq,
        atime: seq,
        mtime: seq,
        ctime: seq,
    };
    Record::put(inode_key(ino).to_vec(), seq, v.encode())
}

/// A header-only node at `ADDR` under `first`'s stamp, then one 4 KiB
/// frame per `(stamp, seq)` in order; returns the tail after each append.
async fn forge_log(
    vol: &NamedTempFile,
    first: FrameStamp,
    frames: &[(FrameStamp, u64)],
) -> Vec<usize> {
    let layout = v2().stamped(first);
    write_node(
        vol.path(),
        &layout,
        &NodeWriteParams {
            node_addr: ADDR,
            node_seq: NODE_SEQ,
            tree_id: TREE_INODES,
            level: 0,
            min_key: b"",
            max_key: &[0xFF; 16],
        },
        &[],
        0,
    )
    .await
    .expect("header-only node");
    let mut tail = NODE_PAGE;
    let mut tails = Vec::new();
    for (i, (s, seq)) in frames.iter().enumerate() {
        let dest = AppendDest {
            node_addr: ADDR,
            node_seq: NODE_SEQ,
            tail_offset: tail,
        };
        tail = append_bset(
            vol.path(),
            &v2().stamped(*s),
            &dest,
            &[put(i as u64 + 1, *seq)],
            *seq,
        )
        .await
        .expect("append");
        tails.push(tail);
    }
    tails
}

/// Overwrite the frame at `at` with one under `s` (the zombie's write at
/// its remembered tail — a position the log may already hold).
async fn overwrite_frame(vol: &NamedTempFile, at: usize, s: FrameStamp, seq: u64) {
    let dest = AppendDest {
        node_addr: ADDR,
        node_seq: NODE_SEQ,
        tail_offset: at,
    };
    append_bset(vol.path(), &v2().stamped(s), &dest, &[put(99, seq)], seq)
        .await
        .expect("zombie append");
}

/// §5.8.2, the keep rule: flush-then-transfer guarantees every legitimate
/// frame of generation `g` precedes `g`'s recorded tail, so a predecessor's
/// frames BEFORE the tail are kept under the successor's screen — and the
/// successor's own frames past it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_predecessors_pre_handover_frames_are_never_screened() {
    let _c = COUNTERS.lock().await;
    let vol = fresh_volume();
    let a1 = stamp(1, 1);
    let b2 = stamp(2, 2);
    let tails = forge_log(&vol, a1, &[(a1, 10), (a1, 11), (b2, 12)]).await;
    // The release at the end of g = 1 recorded the tail after a1's second
    // frame; the successor (g = 2) appended there.
    let screen = FrameScreen {
        g_current: 2,
        appender_current: Some(2),
        recorded_tail: Some((1, tails[1] as u32)),
        pr_fenced: false,
    };
    let before = META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed);
    let loaded = load_node_screened(vol.path(), &v2(), ADDR, 0, Some(&screen))
        .await
        .expect("load");
    assert_eq!(loaded.bset_count(), 3, "every legitimate frame kept");
    assert_eq!(loaded.tail_offset(), tails[2]);
    assert_eq!(loaded.foreign_frames_screened(), 0);
    assert_eq!(
        META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed),
        before
    );
}

/// §5.8.2 rule 2: a frame under an OLDER generation at or past the tail
/// its generation's release recorded is a zombie's append after the slot
/// moved — screened, the log ends before it (an untouched leaf: the
/// successor never appended here).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_past_the_recorded_tail_with_an_older_g_are_screened() {
    let _c = COUNTERS.lock().await;
    let vol = fresh_volume();
    let a1 = stamp(1, 1);
    let tails = forge_log(&vol, a1, &[(a1, 10), (a1, 11), (a1, 12)]).await;
    // The release recorded the tail after the SECOND frame; the third is
    // the zombie's.
    let screen = FrameScreen {
        g_current: 2,
        appender_current: Some(2),
        recorded_tail: Some((1, tails[1] as u32)),
        pr_fenced: false,
    };
    let before = META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed);
    let loaded = load_node_screened(vol.path(), &v2(), ADDR, 0, Some(&screen))
        .await
        .expect("load");
    assert_eq!(
        loaded.bset_count(),
        2,
        "the zombie's frame is not this log's"
    );
    assert_eq!(
        loaded.tail_offset(),
        tails[1],
        "the log ends at the recorded tail"
    );
    assert_eq!(loaded.foreign_frames_screened(), 1);
    assert_eq!(
        META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed),
        before + 1
    );
    // Without a screen (no plane answers for the slot) rule 2 cannot run
    // and the frame is kept — the honest un-screened load.
    let unscreened = load_node_screened(vol.path(), &v2(), ADDR, 0, None)
        .await
        .expect("load");
    assert_eq!(unscreened.bset_count(), 3);
}

/// §5.8.2 rule 3: a frame whose `g` is LOWER than an earlier frame's in
/// the same log was appended by an older lessee after a newer one wrote —
/// screened even with no recorded tail (a leaf the successor minted) and
/// even with no plane at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_monotone_g_in_one_log_is_screened() {
    let _c = COUNTERS.lock().await;
    let vol = fresh_volume();
    let b2 = stamp(2, 2);
    let a1 = stamp(1, 1);
    let tails = forge_log(&vol, b2, &[(b2, 20), (a1, 21)]).await;
    let screen = FrameScreen {
        g_current: 2,
        appender_current: Some(2),
        recorded_tail: None,
        pr_fenced: false,
    };
    for sc in [Some(&screen), None] {
        let loaded = load_node_screened(vol.path(), &v2(), ADDR, 0, sc)
            .await
            .expect("load");
        assert_eq!(
            loaded.bset_count(),
            1,
            "the older-g frame is screened ({sc:?})"
        );
        assert_eq!(loaded.tail_offset(), tails[0]);
        assert_eq!(loaded.foreign_frames_screened(), 1);
    }
}

/// §5.8.2 rule 1: a generation ABOVE the current lease generation is
/// impossible on a healthy plane — `foreign_frames_screened` on a non-PR
/// substrate, the must-stay-0 `appender_fence_breach` under a device
/// fence (a write the reservation should have rejected).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_generation_above_the_current_one_is_a_breach_under_a_device_fence() {
    let _c = COUNTERS.lock().await;
    let vol = fresh_volume();
    let a1 = stamp(1, 1);
    let c3 = stamp(3, 3);
    forge_log(&vol, a1, &[(a1, 10), (c3, 11)]).await;
    let screened0 = META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed);
    let breach0 = META_KV_APPENDER_FENCE_BREACH.load(Ordering::Relaxed);
    let non_pr = FrameScreen {
        g_current: 2,
        appender_current: Some(2),
        recorded_tail: None,
        pr_fenced: false,
    };
    let loaded = load_node_screened(vol.path(), &v2(), ADDR, 0, Some(&non_pr))
        .await
        .expect("load");
    assert_eq!(loaded.bset_count(), 1);
    assert_eq!(
        META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed),
        screened0 + 1
    );
    assert_eq!(
        META_KV_APPENDER_FENCE_BREACH.load(Ordering::Relaxed),
        breach0
    );
    let pr = FrameScreen {
        pr_fenced: true,
        ..non_pr
    };
    let loaded = load_node_screened(vol.path(), &v2(), ADDR, 0, Some(&pr))
        .await
        .expect("load");
    assert_eq!(loaded.bset_count(), 1);
    assert_eq!(
        META_KV_APPENDER_FENCE_BREACH.load(Ordering::Relaxed),
        breach0 + 1
    );
}

/// **Rule 4** (review round 1, Issue 18): a frame stamped the CURRENT
/// generation by an appender that is NOT the slot's lessee passes rules
/// 1–3 (none reads `appender_id`) — yet one generation has ONE lessee by
/// construction, so such a frame is a manager bug or a forged frame: the
/// breach class under a device fence, screened otherwise, like rule 1.
/// The lessee's own frames at the current generation are kept, and a
/// screened rule-4 frame with the LESSEE's frame behind it is the
/// overwrite class, refused loud.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_current_generation_frame_from_a_foreign_appender_is_screened() {
    let _c = COUNTERS.lock().await;
    let vol = fresh_volume();
    let b2 = stamp(2, 2);
    let x2 = stamp(7, 2);
    forge_log(&vol, b2, &[(b2, 10), (x2, 11)]).await;
    let screened0 = META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed);
    let breach0 = META_KV_APPENDER_FENCE_BREACH.load(Ordering::Relaxed);
    let non_pr = FrameScreen {
        g_current: 2,
        appender_current: Some(2),
        recorded_tail: None,
        pr_fenced: false,
    };
    assert_eq!(non_pr.foreign_rule(x2, 8192, Some(2)), Some(4));
    assert_eq!(non_pr.foreign_rule(b2, 8192, Some(2)), None);
    // UNLEASED at `g` (a release keeps `g`): the former lessee's frames
    // and the maintaining manager's are both legitimate — rule 4 is
    // inert, both frames load (the slot-transfer matrix leg's find: a
    // released tree read empty after its remount).
    let unleased = FrameScreen {
        appender_current: None,
        ..non_pr
    };
    assert_eq!(unleased.foreign_rule(x2, 8192, Some(2)), None);
    let loaded = load_node_screened(vol.path(), &v2(), ADDR, 0, Some(&unleased))
        .await
        .expect("load");
    assert_eq!(
        loaded.bset_count(),
        2,
        "nothing screened on an unleased slot"
    );
    assert_eq!(
        META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed),
        screened0
    );
    let loaded = load_node_screened(vol.path(), &v2(), ADDR, 0, Some(&non_pr))
        .await
        .expect("load");
    assert_eq!(
        loaded.bset_count(),
        1,
        "the lessee's frame is kept, the foreign one screened"
    );
    assert_eq!(
        META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed),
        screened0 + 1
    );
    assert_eq!(
        META_KV_APPENDER_FENCE_BREACH.load(Ordering::Relaxed),
        breach0
    );
    let pr = FrameScreen {
        pr_fenced: true,
        ..non_pr
    };
    let loaded = load_node_screened(vol.path(), &v2(), ADDR, 0, Some(&pr))
        .await
        .expect("load");
    assert_eq!(loaded.bset_count(), 1);
    assert_eq!(
        META_KV_APPENDER_FENCE_BREACH.load(Ordering::Relaxed),
        breach0 + 1,
        "under a device fence a foreign current-generation frame is the breach class"
    );
    // The overwrite face: the foreign frame sits BEFORE the lessee's own.
    let vol2 = fresh_volume();
    forge_log(&vol2, b2, &[(b2, 10), (x2, 11), (b2, 12)]).await;
    let before = META_KV_FOREIGN_FRAME_OVERWRITE_DETECTED.load(Ordering::Relaxed);
    let err = load_node_screened(vol2.path(), &v2(), ADDR, 0, Some(&non_pr))
        .await
        .expect_err("a foreign frame under the lessee's frame refuses");
    assert!(
        matches!(err, KvError::Corrupt(ref m) if m.contains("foreign_frame_overwrite_detected")),
        "{err}"
    );
    assert_eq!(
        META_KV_FOREIGN_FRAME_OVERWRITE_DETECTED.load(Ordering::Relaxed),
        before + 1
    );
}

/// §5.8.2 residual class (ii), the after-the-fact face: a zombie's frame at
/// a position the successor had ALREADY written — screened by rule 2 with
/// the successor's own frame BEHIND it — is acked loss; the load refuses
/// loud (`foreign_frame_overwrite_detected`) rather than serving a log
/// with a hole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zombie_frame_under_a_successors_frame_is_an_overwrite_refused_loud() {
    let _c = COUNTERS.lock().await;
    let vol = fresh_volume();
    let a1 = stamp(1, 1);
    let b2 = stamp(2, 2);
    let tails = forge_log(&vol, a1, &[(a1, 10), (b2, 11), (b2, 12)]).await;
    // The zombie overwrites the successor's first frame at the recorded
    // tail (its remembered position).
    overwrite_frame(&vol, tails[0], a1, 13).await;
    let screen = FrameScreen {
        g_current: 2,
        appender_current: Some(2),
        recorded_tail: Some((1, tails[0] as u32)),
        pr_fenced: false,
    };
    let before = META_KV_FOREIGN_FRAME_OVERWRITE_DETECTED.load(Ordering::Relaxed);
    let err = load_node_screened(vol.path(), &v2(), ADDR, 0, Some(&screen))
        .await
        .expect_err("an overwritten acked frame refuses");
    assert!(
        matches!(err, KvError::Corrupt(ref m) if m.contains("foreign_frame_overwrite_detected")),
        "{err}"
    );
    assert_eq!(
        META_KV_FOREIGN_FRAME_OVERWRITE_DETECTED.load(Ordering::Relaxed),
        before + 1
    );
}

/// Frame v1 is byte-identical on a bit-17-absent volume, and the two
/// versions are foreign to each other's layout: a v2 frame on a v1 layout
/// reads as garbage (the log ends before it), a v1 frame on a v2 layout
/// likewise — never misread as a stamped frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frame_v1_stays_byte_identical_and_the_two_versions_are_foreign_to_each_other() {
    let v1 = NodeLayout::new(NODE_SIZE).expect("flat layout");
    let recs = [put(7, 42)];
    let frame = encode_bset_frame(&v1, NODE_SEQ, &recs, 42).expect("v1 frame");
    // The shipped 32 B header, field by field.
    assert_eq!(&frame[0..4], b"KBSF");
    assert_eq!(u16::from_le_bytes([frame[4], frame[5]]), 1);
    assert_eq!(&frame[6..8], &[0, 0]);
    assert_eq!(
        u64::from_le_bytes(frame[8..16].try_into().unwrap()),
        NODE_SEQ
    );
    assert_eq!(
        u32::from_le_bytes(frame[16..20].try_into().unwrap()) as usize,
        frame.len()
    );
    let bset_len = u32::from_le_bytes(frame[20..24].try_into().unwrap()) as usize;
    assert_eq!(
        u64::from_le_bytes(frame[24..32].try_into().unwrap()),
        xxhash_rust::xxh3::xxh3_64(&frame[..24])
    );
    assert_eq!(v1.frame_len(), BSET_FRAME_LEN);
    assert!(
        v1.stamped(stamp(9, 9)) == v1,
        "a flat layout carries no stamp"
    );
    // v2: the same fields, the stamp, the checksum over 32 bytes, the
    // bset at 40.
    let s = stamp(3, 7);
    let frame2 = encode_bset_frame(&v2().stamped(s), NODE_SEQ, &recs, 42).expect("v2 frame");
    assert_eq!(u16::from_le_bytes([frame2[4], frame2[5]]), 2);
    assert_eq!(u32::from_le_bytes(frame2[24..28].try_into().unwrap()), 3);
    assert_eq!(u32::from_le_bytes(frame2[28..32].try_into().unwrap()), 7);
    assert_eq!(
        u64::from_le_bytes(frame2[32..40].try_into().unwrap()),
        xxhash_rust::xxh3::xxh3_64(&frame2[..32])
    );
    assert_eq!(v2().frame_len(), BSET_FRAME_V2_LEN);
    assert_eq!(
        &frame[BSET_FRAME_LEN..BSET_FRAME_LEN + bset_len],
        &frame2[BSET_FRAME_V2_LEN..BSET_FRAME_V2_LEN + bset_len],
        "the embedded bset image is the same under both frames"
    );
    // Cross-layout loads: each version is garbage to the other layout.
    let vol = fresh_volume();
    for (writer, reader) in [(v1, v2()), (v2(), v1)] {
        write_node(
            vol.path(),
            &writer,
            &NodeWriteParams {
                node_addr: ADDR,
                node_seq: NODE_SEQ,
                tree_id: TREE_INODES,
                level: 0,
                min_key: b"",
                max_key: &[0xFF; 16],
            },
            &[],
            0,
        )
        .await
        .expect("header");
        append_bset(
            vol.path(),
            &writer,
            &AppendDest {
                node_addr: ADDR,
                node_seq: NODE_SEQ,
                tail_offset: NODE_PAGE,
            },
            &recs,
            42,
        )
        .await
        .expect("append");
        let own = load_node_screened(vol.path(), &writer, ADDR, 0, None)
            .await
            .expect("own layout loads");
        assert_eq!(own.bset_count(), 1);
        let foreign = load_node_screened(vol.path(), &reader, ADDR, 0, None)
            .await
            .expect("the other layout loads the header");
        assert_eq!(
            foreign.bset_count(),
            0,
            "a v{} frame is foreign to a v{} layout",
            writer.frame_version(),
            reader.frame_version()
        );
        assert_eq!(foreign.tail_offset(), NODE_PAGE);
    }
}

// ===========================================================================
// Part B — tokens (§5.7): the in-process fixture is a stamped volume opened
// by a WRITER under `SQUEEZEFS_SYMMETRIC_META=1` (the manager, the holder),
// its `TokenService` on an RPC listener, and a READER opened read-only on
// the same file with its token client armed against that listener — the
// shape `readonly_mount_tests` uses for the S5 reader with the token plane
// on top. N daemon processes on one volume is PR 12's join ladder.
// ===========================================================================

use squeezefs::cluster_wire as cw;
use squeezefs::meta_backend::kv::backend::{
    test_conveyor_hold_parked, test_conveyor_hold_release, KvMetaBackend,
    TEST_CONVEYOR_HOLD_PRE_DRAIN, TEST_CONVEYOR_HOLD_PRE_ROLLBACK, TEST_CONVEYOR_HOLD_STAGE,
};
use squeezefs::meta_backend::kv::builder::{format_v3_stamped, FormatV3Options};
use squeezefs::meta_backend::kv::slot_lease::SYMMETRIC_META_ENV;
use squeezefs::meta_backend::{
    open_routed_meta_set, open_routed_meta_set_read_only, plan_meta_slot_set, Metadata,
    RoutedMetaBackend,
};
use squeezefs::meta_ship::token_plane::{
    LeaseVerdict, RecallDataSink, RecalledObject, TokenClientConfig, TokenReaderPlane,
    TokenService, TokenWants,
};
use squeezefs::{free_grace, ro_coherence};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

const VOL_LEN: u64 = 64 * 1024 * 1024;
const RING_LEN: u64 = 1024 * 1024;
const SECRET: &[u8] = b"sym-coherence-tests-enroll-secret";

/// The knobs are process-global; every token contract serializes on it.
static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn format_stamped(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format stamped volume");
    p
}

/// Open the writer ARMED (`SQUEEZEFS_SYMMETRIC_META=1` on the stamped
/// volume; the knobs cleared after — a mount reads them once) — the
/// ROUTED set, the mount's own shape (global inos, the mint policy).
async fn open_armed_writer(path: &std::path::Path) -> Arc<RoutedMetaBackend> {
    std::env::set_var(SYMMETRIC_META_ENV, "1");
    std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
    let r = open_routed_meta_set(&[path.display().to_string()]).await;
    std::env::remove_var(SYMMETRIC_META_ENV);
    std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
    let w = r.expect("armed writer open");
    assert!(
        w.volumes[0].slot_lease_armed(),
        "the plane must arm on the stamped volume"
    );
    assert!(
        w.volumes[0].token_holder().is_some(),
        "an armed writer is a token holder"
    );
    w
}

async fn shutdown(routed: &RoutedMetaBackend) {
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

fn listener_cfg() -> cw::RpcListenerConfig {
    cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    }
}

/// The holder's token service on its own listener.
fn holder_listener(vol: &Arc<KvMetaBackend>) -> (Arc<cw::RpcListener>, String) {
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        TokenService::new(Arc::clone(vol)),
    )
    .expect("token listener");
    let endpoint = host.endpoint().to_string();
    (host, endpoint)
}

/// A reader opened read-only on the volume with its token client armed
/// against `endpoint`, its recall channel FRESH before it returns.
async fn open_token_reader(
    path: &std::path::Path,
    endpoint: &str,
    client_id: &str,
) -> (Arc<RoutedMetaBackend>, Arc<TokenReaderPlane>) {
    let reader = open_routed_meta_set_read_only(&[path.display().to_string()])
        .await
        .expect("read-only open");
    let plane = reader.volumes[0]
        .arm_token_reader(TokenClientConfig {
            endpoint: endpoint.to_string(),
            secret: SECRET.to_vec(),
            client_id: client_id.to_string(),
            volume: 0,
        })
        .expect("token client arms on a read-only open");
    wait_until("the recall channel completes its first round", || {
        plane.stats().channel_fresh
    })
    .await;
    (reader, plane)
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !cond() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A `RecallDataSink` the contracts park: the ack cannot travel while it
/// is held, and every call is counted.
struct ProbeSink {
    parked: std::sync::atomic::AtomicBool,
    release: squeezefs_ipc::sqz_notify::Notify,
    calls: AtomicU64,
    objects: AtomicU64,
}

impl ProbeSink {
    fn new(parked: bool) -> Arc<Self> {
        Arc::new(Self {
            parked: std::sync::atomic::AtomicBool::new(parked),
            release: squeezefs_ipc::sqz_notify::Notify::new(),
            calls: AtomicU64::new(0),
            objects: AtomicU64::new(0),
        })
    }
    fn release(&self) {
        self.parked.store(false, Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

impl RecallDataSink for ProbeSink {
    fn drain_and_purge<'a>(
        &'a self,
        objects: &'a [RecalledObject],
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.objects
                .fetch_add(objects.len() as u64, Ordering::SeqCst);
            loop {
                let notified = self.release.notified();
                if !self.parked.load(Ordering::SeqCst) {
                    break;
                }
                notified.await;
            }
        })
    }
}

/// Gate 5 / R-SYM-4: a foreign create is visible at the reader's NEXT
/// resolve — exact, never bounded, no poll between — because the holder
/// recalled the directory's token before the create committed and the
/// reader's next lookup re-fetched the dentry set; a foreign setattr the
/// same way through the file's own token. `reader_staleness_bound_ms`
/// reads 0 for metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_create_is_visible_at_the_readers_next_resolve() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let pre = Metadata::create(writer.as_ref(), 1, "pre", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-1").await;
    let holder = writer.volumes[0].token_holder().unwrap().clone();

    // First touch: the root's token (dentries) + the child's — two grants,
    // then a RAM hit, then an exact negative.
    let got = Metadata::lookup(reader.as_ref(), 1, "pre").await.unwrap();
    assert_eq!(got.ino, pre.ino);
    assert_eq!(plane.stats().grants, 2, "root (dentries) + child");
    let _ = Metadata::getattr(reader.as_ref(), pre.ino).await.unwrap();
    assert_eq!(
        plane.stats().grants,
        2,
        "the second read of the child is a hit"
    );
    assert!(plane.stats().hits >= 1);
    let missing = Metadata::lookup(reader.as_ref(), 1, "a").await;
    assert!(
        missing.is_err(),
        "an exact negative from the token's dentry set"
    );
    assert_eq!(holder.outstanding(), 2);

    // The foreign create: the commit recalls the root's token and waits
    // for the reader's ack; when `create` returns the reader's very next
    // resolve sees it.
    let recalls0 = holder.stats().recalls;
    let a = Metadata::create(writer.as_ref(), 1, "a", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let hs = holder.stats();
    assert!(
        hs.recalls > recalls0,
        "the create recalled the directory's token"
    );
    assert_eq!(hs.recall_acks, hs.recalls, "every recall was acked");
    assert_eq!(hs.expired_with_lease, 0);
    assert_eq!(hs.timeouts_live, 0);
    let seen = Metadata::lookup(reader.as_ref(), 1, "a").await.unwrap();
    assert_eq!(
        seen.ino, a.ino,
        "visible at the NEXT resolve — no poll in between"
    );
    let rs = plane.stats();
    assert_eq!(rs.recalls_received, rs.recalls_acked);

    // A foreign setattr: the file's own token is recalled.
    Metadata::setattr(
        writer.as_ref(),
        a.ino,
        Some(0o600),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let after = Metadata::getattr(reader.as_ref(), a.ino).await.unwrap();
    assert_eq!(
        after.mode & 0o777,
        0o600,
        "the setattr is exact at the next getattr"
    );

    // A foreign unlink: the directory's token again, and the file's
    // (the record survives at nlink 0 until its destroy — POSIX's
    // unlinked-but-open shape — and the token says so exactly).
    Metadata::unlink(writer.as_ref(), 1, "pre").await.unwrap();
    assert!(Metadata::lookup(reader.as_ref(), 1, "pre").await.is_err());
    assert_eq!(
        Metadata::getattr(reader.as_ref(), pre.ino)
            .await
            .unwrap()
            .nlink,
        0,
        "the unlinked inode's token is recalled and re-granted at nlink 0"
    );
    Metadata::destroy_inode(writer.as_ref(), pre.ino)
        .await
        .unwrap();
    assert!(
        Metadata::getattr(reader.as_ref(), pre.ino).await.is_err(),
        "a destroyed object's grant answers Gone"
    );

    assert_eq!(
        ro_coherence::metadata_staleness_bound_ms(&reader.volumes),
        0,
        "reader_staleness_bound_ms is 0 for metadata under tokens"
    );
    assert!(
        ro_coherence::reader_staleness_bound().as_millis() > 0,
        "the S5 control-plane bound keeps its own number"
    );
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// §5.7.1 — the recall rides the commit: a commit that mutates a token's
/// object cannot be observed (its `create` cannot return) before the
/// reader has acked; the reader's cached records are gone before the ack
/// and the mount's data drain + purge sink runs BEFORE it (the sink held
/// parked holds the commit).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_is_recalled_before_the_conflicting_commit_lands() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-1").await;
    let sink = ProbeSink::new(true);
    assert!(plane.install_data_sink(sink.clone()));
    let holder = writer.volumes[0].token_holder().unwrap().clone();

    assert!(Metadata::lookup(reader.as_ref(), 1, "b").await.is_err());
    assert!(plane.holds(1), "the root's token is cached");

    let w = Arc::clone(&writer);
    let create = tokio::spawn(async move {
        Metadata::create(w.as_ref(), 1, "b", libc::S_IFREG | 0o644, 0, 0).await
    });
    // The recall reached the reader: its entry is gone and the sink is
    // parked — the commit is NOT observable.
    wait_until("the reader's sink was entered", || {
        sink.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(
        !plane.holds(1),
        "the recalled entry left the cache before the ack"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !create.is_finished(),
        "the create cannot land before the ack"
    );
    assert_eq!(holder.stats().recall_acks, 0);
    assert_eq!(
        holder.holders(1),
        1,
        "the grant is outstanding until the ack"
    );

    sink.release();
    let b = create.await.unwrap().unwrap();
    assert_eq!(holder.stats().recall_acks, 1);
    assert_eq!(holder.holders(1), 0);
    let seen = Metadata::lookup(reader.as_ref(), 1, "b").await.unwrap();
    assert_eq!(seen.ino, b.ino);
    plane.stop().await;
    host.shutdown();
    // The holder's clean leave DISARMS the process-wide recall gate
    // (review round 1, Issue 20 — the leave is `disarm_recall_gate`'s
    // production caller): armed while the volume holds tokens, off after.
    assert!(free_grace::recall_gate_armed());
    shutdown(&writer).await;
    assert!(
        !free_grace::recall_gate_armed(),
        "the last holder's leave clears the gate"
    );
}

/// **The first-touch grant ∥ pass race** (review round 1, Issue 2 — the
/// central law's hole). Schedule: a pass mutating the root (a create)
/// computes its recall union while NO reader holds the root, and is held
/// between that union and its apply; a reader's FIRST-TOUCH grant on the
/// root is served inside the hold; the pass then applies. Before the fix
/// the grant read the pre-commit records and was recorded AFTER its read,
/// so the pass — which saw no holder — never recalled it: the reader
/// cached a stale dentry set for ever. The law: the grant registers
/// BEFORE it reads and PARKS while the object is in flight
/// (`dlm_token_grant_parks` +1), and the reader sees the committed record
/// at its next resolve — exact, with no recall needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_touch_grant_inside_the_pass_window_serves_the_committed_records() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-ft").await;
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    assert!(
        !plane.holds(1),
        "nothing cached yet — the grant is a first touch"
    );
    assert_eq!(holder.holders(1), 0);

    // Hold the pass AFTER its recall union (which sees no holder) and
    // BEFORE its apply.
    let parked0 = squeezefs::meta_backend::kv::backend::test_conveyor_post_recall_parked();
    TEST_CONVEYOR_HOLD_STAGE.store(
        squeezefs::meta_backend::kv::backend::TEST_CONVEYOR_HOLD_POST_RECALL,
        Ordering::SeqCst,
    );
    let w = Arc::clone(&writer);
    let create = tokio::spawn(async move {
        Metadata::create(w.as_ref(), 1, "late", libc::S_IFREG | 0o644, 0, 0).await
    });
    wait_until("the pass parked in the post-recall window", || {
        squeezefs::meta_backend::kv::backend::test_conveyor_post_recall_parked() > parked0
    })
    .await;
    assert_eq!(
        holder.stats().recalls,
        0,
        "the union saw no holder: nothing was recalled"
    );

    // The reader's first-touch grant on the root, inside the window.
    let r = Arc::clone(&reader);
    let lookup = tokio::spawn(async move { Metadata::lookup(r.as_ref(), 1, "late").await });
    wait_until(
        "the grant registered and parked on the in-flight object",
        || holder.stats().grant_parks >= 1,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !lookup.is_finished(),
        "a grant on an object a pass holds in flight is served only after the apply"
    );

    // Release the pass: it applies and settles; the parked grant reads the
    // committed records.
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    let created = create.await.unwrap().unwrap();
    let seen = lookup
        .await
        .unwrap()
        .expect("the reader's first resolve sees the committed create — exact");
    assert_eq!(seen.ino, created.ino);
    assert!(plane.holds(1), "the root's token is cached, post-commit");
    assert_eq!(holder.holders(1), 1, "the grant is in the holder table");
    // And the cache stays exact: a second foreign create recalls it.
    let c2 = Metadata::create(writer.as_ref(), 1, "later", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    assert_eq!(
        Metadata::lookup(reader.as_ref(), 1, "later")
            .await
            .unwrap()
            .ino,
        c2.ino
    );
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// §5.7.1 — the batching law: a conveyor pass recalls the UNION of its
/// batch's objects ONCE. Sixteen creates held into one pass under the
/// conveyor's pre-drain seam recall the directory's token once (one
/// batch, one recall, one ack).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recall_storm_on_one_object_is_one_batch_per_pass() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-1").await;
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    assert!(Metadata::lookup(reader.as_ref(), 1, "none").await.is_err());
    assert!(plane.holds(1));

    let s0 = holder.stats();
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_DRAIN, Ordering::SeqCst);
    let mut tasks = Vec::new();
    for i in 0..16usize {
        let w = Arc::clone(&writer);
        tasks.push(tokio::spawn(async move {
            Metadata::create(
                w.as_ref(),
                1,
                &format!("s_{i}"),
                libc::S_IFREG | 0o644,
                0,
                0,
            )
            .await
        }));
        let want = i + 1;
        wait_until("committer enqueued behind the held pass", || {
            writer.volumes[0].conveyor_pending_len() >= want
        })
        .await;
    }
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    let s1 = holder.stats();
    assert_eq!(
        s1.recall_batches - s0.recall_batches,
        1,
        "one pass, one batch"
    );
    assert_eq!(
        s1.recalls - s0.recalls,
        1,
        "sixteen creates, ONE recall of the directory"
    );
    assert_eq!(s1.recall_acks - s0.recall_acks, 1);
    for i in 0..16 {
        Metadata::lookup(reader.as_ref(), 1, &format!("s_{i}"))
            .await
            .unwrap_or_else(|e| panic!("s_{i} must be visible at the next resolve: {e}"));
    }
    assert_eq!(
        plane.stats().grants,
        1 + 1 + 16,
        "the root twice (before and after the recall) + sixteen children"
    );
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// §5.7.3 — a reader never acks a recall with a READ in flight: the ack
/// waits on the OBSERVED drain of every data serve that began under the
/// recalled records (`ro_coherence::ServeStamp` — the DMA hazard's
/// ledger, the sink the mount installs runs it), so a serve held open
/// holds the ack and therefore the commit; dropping it releases both.
/// The token entry itself carries no in-flight count (review round 1,
/// Issue 15): its records are immutable and a metadata serve that began
/// before the recall is linearizable at its start — the ledger below is
/// the one drain there is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_never_acks_a_recall_with_a_read_in_flight() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-1").await;
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    // The mount's sink: the observed drain of the serve ledger (no router
    // — the purge half is the census walk the mount adds).
    struct DrainSink;
    impl RecallDataSink for DrainSink {
        fn drain_and_purge<'a>(
            &'a self,
            _objects: &'a [RecalledObject],
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(ro_coherence::drain_in_flight_serves())
        }
    }
    assert!(plane.install_data_sink(Arc::new(DrainSink)));
    ro_coherence::test_arm_serve_ledger();
    let _ = Metadata::getattr(reader.as_ref(), 1).await.unwrap();
    assert!(plane.holds(1));

    // A data serve in flight under the current records (a read of the
    // root's bytes mid-DMA).
    let stamp = ro_coherence::ServeStamp::begin();
    let w = Arc::clone(&writer);
    let create = tokio::spawn(async move {
        Metadata::create(w.as_ref(), 1, "c", libc::S_IFREG | 0o644, 0, 0).await
    });
    wait_until("the recall reached the reader", || {
        plane.stats().recalls_received == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!plane.holds(1), "the entry left the cache at the recall");
    assert_eq!(
        plane.stats().recalls_acked,
        0,
        "no ack while a serve is in flight"
    );
    assert!(!create.is_finished(), "the commit waits on the ack");
    drop(stamp);
    create.await.unwrap().unwrap();
    assert_eq!(plane.stats().recalls_acked, 1);
    assert_eq!(holder.stats().recall_acks, 1);
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// §13 R20 — a reader past `T_self` serves nothing from cache: every
/// token is dropped and the read refuses (fail-closed), never a stale
/// answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_past_t_self_serves_nothing_from_cache() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-1").await;
    let _ = Metadata::getattr(reader.as_ref(), 1).await.unwrap();
    assert!(plane.holds(1));
    plane.test_set_lease_live(Some(false));
    let err = Metadata::getattr(reader.as_ref(), 1)
        .await
        .expect_err("past T_self nothing serves");
    assert!(err.to_string().contains("T_self"), "{err}");
    assert!(!plane.holds(1), "the cache is dropped with the lease");
    assert_eq!(plane.stats().cached, 0);
    assert!(plane.stats().serve_refusals >= 1);
    plane.test_set_lease_live(Some(true));
    let _ = Metadata::getattr(reader.as_ref(), 1)
        .await
        .expect("a live lease serves again (re-granted)");
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// §5.7.1 — a recall completes when the reader acks OR when the
/// membership owner sees its LEASE EXPIRED — never at a timer's deadline
/// (review round 1, Issue 5). A dead reader (`test_die` — its channel
/// gone without a release) holds the root and 64 file tokens; a create in
/// the root recalls the root's token while the oracle still calls the
/// lease LIVE, then the lease expires: the commit completes AT the
/// expiry — well inside the 1 s deadline — `expired_with_lease` counts
/// the recall, `timeouts_live` (the stuck-reader class) does not, the
/// closure `recalls ≡ acks + expired_with_lease` holds, and the dead
/// reader's 64 OTHER grants are swept with it (`lease_swept_grants`), so a
/// second pass on one of those files waits for nobody. The same shape
/// under a lease the oracle keeps LIVE past the deadline counts
/// `timeouts_live` and opens the free-grace recall window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unacked_recall_completes_at_the_readers_lease_expiry() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    std::env::set_var("SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS", "1000");
    let verdict = Arc::new(std::sync::Mutex::new(LeaseVerdict::Live));
    let v = Arc::clone(&verdict);
    holder.install_lease_oracle(Arc::new(move |_client: &str| {
        *v.lock().unwrap_or_else(|p| p.into_inner())
    }));
    const N: usize = 64;
    let mut files = Vec::with_capacity(N);
    for i in 0..N {
        files.push(
            Metadata::create(
                writer.as_ref(),
                1,
                &format!("f{i}"),
                libc::S_IFREG | 0o644,
                0,
                0,
            )
            .await
            .unwrap()
            .ino,
        );
    }

    // A dead reader: the root's and the 64 files' tokens are held at the
    // holder, its channel gone WITHOUT a release (`die` — never the clean
    // leave's `stop`).
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-dead").await;
    let _ = Metadata::getattr(reader.as_ref(), 1).await.unwrap();
    for ino in &files {
        let _ = Metadata::getattr(reader.as_ref(), *ino).await.unwrap();
    }
    assert_eq!(holder.holders(1), 1);
    assert_eq!(holder.outstanding(), (N + 1) as u64);
    plane.test_die();
    tokio::time::sleep(Duration::from_millis(1_200)).await; // past the channel's park bound
    assert_eq!(
        holder.holders(1),
        1,
        "a dead reader's grant stays while its lease is live"
    );

    // The lease expires 300 ms into the recall.
    let v = Arc::clone(&verdict);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        *v.lock().unwrap_or_else(|p| p.into_inner()) = LeaseVerdict::Expired;
    });
    let t0 = std::time::Instant::now();
    Metadata::create(writer.as_ref(), 1, "d", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let wall = t0.elapsed();
    let hs = holder.stats();
    assert_eq!(
        hs.expired_with_lease, 1,
        "the recall completed with the lease"
    );
    assert_eq!(hs.timeouts_live, 0, "never the stuck-reader class");
    assert_eq!(
        hs.recalls,
        hs.recall_acks + hs.expired_with_lease,
        "the closure law"
    );
    assert_eq!(holder.holders(1), 0, "the dead reader's grant is retired");
    assert!(
        wall >= Duration::from_millis(280) && wall < Duration::from_millis(900),
        "the commit completed at the lease's expiry, never at the 1 s deadline: {wall:?}"
    );
    assert_eq!(
        hs.lease_swept_grants, N as u64,
        "the dead reader's other grants were swept with its lease"
    );
    assert_eq!(holder.outstanding(), 0);
    // The second pass, on one of the swept files: nobody to recall.
    let t1 = std::time::Instant::now();
    Metadata::setattr(
        writer.as_ref(),
        files[7],
        Some(0o640),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        t1.elapsed() < Duration::from_millis(200),
        "the second pass never waits on the dead reader"
    );
    assert_eq!(
        holder.stats().recalls,
        hs.recalls,
        "no new recall was issued"
    );
    assert_eq!(
        free_grace::recall_gate_verdict(),
        free_grace::RecallGate::Gated
    );

    // The LIVE shape: a second dead reader whose lease the oracle keeps
    // live past the deadline — the tripwire class, and the free path takes
    // the ring.
    *verdict.lock().unwrap() = LeaseVerdict::Live;
    let (reader2, plane2) = open_token_reader(&path, &endpoint, "reader-stuck").await;
    let _ = Metadata::getattr(reader2.as_ref(), 1).await.unwrap();
    plane2.test_die();
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let (clock, _ticks) = {
        let ticks = Arc::new(AtomicU64::new(10_000));
        (
            squeezefs::membership::LeaseClock::manual(Arc::clone(&ticks)),
            ticks,
        )
    };
    free_grace::arm_owner_plane_with(
        clock,
        Duration::from_millis(4_000),
        Duration::from_millis(2_000),
    );
    let t2 = std::time::Instant::now();
    Metadata::create(writer.as_ref(), 1, "e", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    assert!(
        t2.elapsed() >= Duration::from_millis(900),
        "a LIVE lease that never acks waits the whole deadline"
    );
    let hs = holder.stats();
    assert_eq!(hs.timeouts_live, 1);
    assert_eq!(hs.expired_with_lease, 1);
    // With no installed membership owner `T_owner` reads 0, so the window
    // the live timeout opened is already closed: the gate stays Gated
    // (the ring's role needs the plane the mount arms).
    let _ = free_grace::recall_gate_verdict();
    free_grace::disarm_owner_plane();
    std::env::remove_var("SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS");
    host.shutdown();
    shutdown(&writer).await;
}

/// §5.7.3 — recall-driven free-grace: with the holder armed a terminal
/// free is RECALL-GATED (publishes directly — the recall was the
/// qualification), while a free issued inside a live-timeout window rides
/// the ring (`timeout_deferrals`) and publishes when the window closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_free_never_ships_before_every_recall_is_acked_or_expired() {
    let _g = SEAM.lock().await;
    free_grace::reset_for_test();
    let ticks = Arc::new(AtomicU64::new(10_000));
    let clock = squeezefs::membership::LeaseClock::manual(Arc::clone(&ticks));
    free_grace::arm_owner_plane_with(
        clock,
        Duration::from_millis(4_000),
        Duration::from_millis(2_000),
    );
    // A member holds the ring's bound at 0: every free would be held.
    free_grace::publish_bound(0, 1);
    assert!(free_grace::armed());
    let ba = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new("recall-gate")
            .await
            .unwrap(),
    );
    // Unarmed gate: the shipped ring path holds the offset.
    assert_eq!(
        free_grace::recall_gate_verdict(),
        free_grace::RecallGate::Off
    );
    let b0 = ba.allocate_block().await.unwrap();
    ba.free_block(b0).await.unwrap();
    assert_eq!(
        ba.grace_len(),
        1,
        "the S5 ring holds a free under an unacked member"
    );

    // Armed gate: the recall was the qualification — a free publishes
    // directly.
    free_grace::arm_recall_gate();
    let gated0 = free_grace::recall_gated_frees();
    let b1 = ba.allocate_block().await.unwrap();
    ba.free_block(b1).await.unwrap();
    assert_eq!(ba.grace_len(), 1, "the recall-gated free bypassed the ring");
    assert_eq!(free_grace::recall_gated_frees(), gated0 + 1);
    assert!(
        ba.free_block_indices().contains(&(b1 / ba.chunk_size())),
        "published to the free list at once"
    );

    // A live-timeout window: the ring is the timeout path.
    free_grace::test_open_recall_window_ms(1_000);
    let def0 = free_grace::timeout_deferrals();
    let b2 = ba.allocate_block().await.unwrap();
    ba.free_block(b2).await.unwrap();
    assert_eq!(ba.grace_len(), 2, "a free inside the window rides the ring");
    assert_eq!(free_grace::timeout_deferrals(), def0 + 1);
    // The window closes: gated again.
    ticks.fetch_add(1_001, Ordering::SeqCst);
    assert_eq!(
        free_grace::recall_gate_verdict(),
        free_grace::RecallGate::Gated
    );
    // The closure law over the ring's own ledger holds throughout.
    assert_eq!(
        free_grace::deferrals(),
        free_grace::releases() + ba.grace_len() as u64
    );
    free_grace::disarm_recall_gate();
    free_grace::disarm_owner_plane();
    free_grace::reset_for_test();
}

/// §5.7.4 / gate 5 — the broadcast shape: one writer, R = 64 readers of one
/// object; the holder's publish recalls all 64 in ONE batch
/// (`dlm_token_recall_fanout` p99 = 64), every one acked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_broadcast_shape_recalls_every_reader_once_per_publish() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    let f = Metadata::create(writer.as_ref(), 1, "hot", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    // The planes dial the holder directly (no routed layer between): the
    // object is the file's LOCAL key ino on volume 0.
    let (_v, local) = writer.route_ino(f.ino);
    const R: usize = 64;
    let mut planes = Vec::with_capacity(R);
    for i in 0..R {
        let plane = TokenReaderPlane::new(TokenClientConfig {
            endpoint: endpoint.clone(),
            secret: SECRET.to_vec(),
            client_id: format!("reader-{i}"),
            volume: 0,
        });
        let task = Arc::clone(&plane);
        tokio::spawn(async move { task.run_recall_channel().await });
        planes.push(plane);
    }
    for p in &planes {
        let p = Arc::clone(p);
        wait_until("channel fresh", || p.stats().channel_fresh).await;
        let serve = p
            .serve(local, TokenWants::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(serve.entry().attrs.mode & 0o777, 0o644);
    }
    assert_eq!(holder.holders(local), R);
    let s0 = holder.stats();
    Metadata::setattr(
        writer.as_ref(),
        f.ino,
        Some(0o640),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let s1 = holder.stats();
    assert_eq!(s1.recall_batches - s0.recall_batches, 1);
    assert_eq!(s1.recalls - s0.recalls, R as u64);
    assert_eq!(s1.recall_acks - s0.recall_acks, R as u64);
    assert_eq!(s1.timeouts_live, 0);
    assert_eq!(
        s1.fanout_p99, R as u64,
        "p99 of the fan-out is the reader count"
    );
    // The scoping row for the evidence note (§5.7.3: under tokens the
    // free's hold IS the recall round trip — `free_grace_hold_ms` ≈
    // `dlm_token_recall_rtt_ns.total`); read with `--nocapture`.
    let rtt = holder.rtt_json();
    eprintln!(
        "SCOPING broadcast R={R}: dlm_token_recall_rtt_ns={rtt} dlm_token_recall_fanout={}",
        holder.fanout_json()
    );
    let sum = |phase: &str| rtt[phase]["sum_ns"].as_u64().expect("phase sum");
    assert_eq!(
        sum("send") + sum("drain") + sum("ack"),
        sum("total"),
        "dlm_token_recall_rtt_ns is EXACT-SUM per batch"
    );
    assert_eq!(rtt["total"]["count"].as_u64(), Some(1), "one batch");
    assert_eq!(holder.holders(local), 0);
    for p in &planes {
        let serve = p
            .serve(local, TokenWants::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serve.entry().attrs.mode & 0o777,
            0o640,
            "exact at the next resolve"
        );
        p.stop().await;
    }
    host.shutdown();
    shutdown(&writer).await;
}

/// **A voluntary release runs the recall's drain + purge FIRST** (review
/// round 1, Issue 4). The reader's records budget is set so the second
/// token evicts the first: the eviction must run the installed data sink
/// (the in-flight serve drain + the R-6 block-key purge) on the retired
/// object BEFORE the holder is told — with the sink parked, the holder's
/// `releases` stays put; released, the `Release` lands. Before the fix
/// the eviction sent the release straight away and the reader's layout /
/// block-key caches kept the released object for their horizon while the
/// holder's next free — which recalls nobody for a released object —
/// published directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_voluntary_release_drains_and_purges_before_the_holder_is_told() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    let a = Metadata::create(writer.as_ref(), 1, "a", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let b = Metadata::create(writer.as_ref(), 1, "b", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-evict").await;
    let sink = ProbeSink::new(false);
    assert!(plane.install_data_sink(sink.clone()));
    let (_v, a_local) = writer.route_ino(a);
    let (_v, b_local) = writer.route_ino(b);

    let _ = Metadata::getattr(reader.as_ref(), a).await.unwrap();
    let held = plane.stats().cached_bytes;
    assert!(held > 0, "the entry is charged");
    assert!(plane.holds(a_local));
    // A budget that holds ONE such entry: the next grant evicts `a`.
    plane.test_set_records_budget(Some(held + held / 2));
    sink.parked.store(true, Ordering::SeqCst);
    let r = Arc::clone(&reader);
    let fetch_b = tokio::spawn(async move { Metadata::getattr(r.as_ref(), b).await });
    wait_until("the eviction entered the data sink", || {
        sink.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(
        sink.objects.load(Ordering::SeqCst),
        1,
        "the retired object was purged"
    );
    assert!(!plane.holds(a_local), "the evicted entry left the cache");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        holder.stats().releases,
        0,
        "the holder is told only after the drain + purge"
    );
    assert_eq!(holder.holders(a_local), 1);
    sink.release();
    fetch_b.await.unwrap().unwrap();
    wait_until("the release reached the holder", || {
        holder.stats().releases == 1
    })
    .await;
    assert_eq!(holder.holders(a_local), 0);
    assert!(plane.holds(b_local));
    assert!(
        plane.stats().cached_bytes <= held + held / 2,
        "the cache sits inside its budget"
    );
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// **The records budget is BYTES on the R5 component** (review round 1,
/// Issue 7): every entry is charged its encoded records (attrs, xattrs,
/// the dentry set) — `dlm_token_cached_bytes` is live — the cache evicts
/// by bytes, and ONE entry larger than the whole budget is refused loud
/// (`dlm_token_oversize_refusals`), never resident beyond it. The
/// derivation (1/256 of the R5 budget) is tie-tested in
/// `derivation_sweep_tests`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_token_cache_is_bounded_by_bytes_and_refuses_an_oversize_entry() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    // A directory of 200 names — its token carries them all.
    let d = Metadata::create(writer.as_ref(), 1, "big", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    for i in 0..200 {
        Metadata::create(
            writer.as_ref(),
            d,
            &format!("entry-{i:04}"),
            libc::S_IFREG | 0o644,
            0,
            0,
        )
        .await
        .unwrap();
    }
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-bytes").await;
    let (_v, d_local) = writer.route_ino(d);
    assert_eq!(plane.stats().cached_bytes, 0);
    let page = Metadata::readdir(reader.as_ref(), d, 0, 1024)
        .await
        .unwrap();
    assert!(page.len() >= 200, "the whole set is served");
    let bytes = plane.stats().cached_bytes;
    assert!(
        bytes > 200 * 24,
        "the directory's entry is charged its dentries: {bytes} B"
    );
    // Recall credits the bytes back.
    Metadata::create(writer.as_ref(), d, "one-more", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    wait_until("the recall dropped the directory's entry", || {
        !plane.holds(d_local)
    })
    .await;
    assert_eq!(plane.stats().cached_bytes, 0, "credited at the recall");
    // A budget below the directory's records: the grant is refused loud,
    // nothing resident.
    plane.test_set_records_budget(Some(bytes / 2));
    let err = Metadata::readdir(reader.as_ref(), d, 0, 1024)
        .await
        .expect_err("an entry larger than the budget is never resident")
        .to_string();
    assert!(
        err.contains("token records budget"),
        "the refusal names the budget: {err}"
    );
    assert_eq!(plane.stats().oversize_refusals, 1);
    assert_eq!(plane.stats().cached_bytes, 0);
    assert!(!plane.holds(d_local));
    plane.test_set_records_budget(None);
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// **The recall purge is PER INO** (review round 1, Issue 10): the
/// mount's recall sink drops the recalled object's layout from the
/// reader's router cache and purges exactly the block keys its layout
/// names — every other object's cached blocks SURVIVE the recall, so a
/// reader on a shared hot tree keeps its warm read tier. The whole
/// census stays the fallback for an object whose layout the reader
/// cannot enumerate (counted: `dlm_token_recall_census_purges`, 0 here).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recall_purges_the_recalled_objects_block_keys_and_leaves_the_rest() {
    use squeezefs::layout_wire::{encode_layout, LayoutMetadata};
    use squeezefs::meta_ship::token_plane::{recall_purge_counts, MountRecallSink};
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let striped = |keys: &[&str]| LayoutMetadata {
        file_type: "striped".to_string(),
        size: 4096 * keys.len() as u64,
        block_map_id: None,
        block_prefix: None,
        file_id: None,
        data_key: None,
        block_map: Some(
            keys.iter()
                .enumerate()
                .map(|(b, k)| (b as u32, k.to_string()))
                .collect(),
        ),
    };
    let x = Metadata::create(writer.as_ref(), 1, "x", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let y = Metadata::create(writer.as_ref(), 1, "y", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let kx = ["nvme://vol-tok/4096", "nvme://vol-tok/8192:0:100"];
    let ky = ["nvme://vol-tok/12288"];
    writer
        .set_layout_and_size(x, &encode_layout(&striped(&kx)).unwrap(), 8192, &[])
        .await
        .unwrap();
    writer
        .set_layout_and_size(y, &encode_layout(&striped(&ky)).unwrap(), 4096, &[])
        .await
        .unwrap();

    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-scoped").await;
    let (router, _dev) = data_router("vol-tok").await;
    router.set_meta_backend(Arc::clone(&reader));
    // The reader's warm read tier: X's two blocks (one a decorated
    // mapping — the tier keys on the STORED value) and Y's one.
    for k in kx.iter().chain(ky.iter()) {
        router
            .cache
            .read_lru
            .put(k, bytes::Bytes::from_static(b"warm"));
    }
    router
        .cache
        .hot_block
        .put(ky[0], bytes::Bytes::from_static(b"hot"));
    assert!(plane.install_data_sink(MountRecallSink::new(router.clone(), 0)));
    let before = recall_purge_counts();

    let _ = Metadata::getattr(reader.as_ref(), x).await.unwrap();
    let _ = Metadata::getattr(reader.as_ref(), y).await.unwrap();
    let (_v, x_local) = writer.route_ino(x);
    assert!(plane.holds(x_local));

    // The holder's writeback publish on X displaces its blocks — the
    // conflicting commit recalls X's token; the reader acks after
    // purging X's keys and nothing else.
    let kx2 = ["nvme://vol-tok/16384", "nvme://vol-tok/20480:0:100"];
    writer
        .set_layout_and_size(x, &encode_layout(&striped(&kx2)).unwrap(), 8192, &[])
        .await
        .unwrap();
    wait_until("the recall of X was acked", || {
        plane.stats().recalls_acked >= 1
    })
    .await;
    assert!(!plane.holds(x_local));
    for k in kx {
        assert!(
            router.cache.read_lru.get(k).is_none(),
            "X's block key {k} left the read tier at the recall"
        );
    }
    assert!(
        router.cache.read_lru.get(ky[0]).is_some(),
        "Y's cached block SURVIVES a recall of X"
    );
    assert!(
        router.cache.hot_block.get(ky[0]).is_some(),
        "Y's hot block survives too"
    );
    let after = recall_purge_counts();
    assert_eq!(
        after.scoped - before.scoped,
        1,
        "one object purged by its layout"
    );
    assert_eq!(after.census - before.census, 0, "no census fallback");
    // X's two stored values + the decorated mapping's free-able base
    // (the tiers key on the stored value; the base is purged beside it).
    assert_eq!(
        after.keys - before.keys,
        kx.len() as u64 + 1,
        "exactly X's block keys were purged"
    );
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// **The holder answers `NotHolder` for a slot it does not lease**
/// (review round 1, Issue 12): a wire joiner acquires a slot; a reader's
/// grant on an object of that slot is answered `NotHolder { holder }` —
/// one lease-table read BEFORE anything is registered or read — and the
/// reader fails closed naming the holder (PR 12's redirect trigger),
/// never a grant of this manager's stale-by-construction view of a tree
/// another appender is RAM-authoritative for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_on_a_slot_another_appender_leases_is_answered_not_holder() {
    use squeezefs::meta_backend::guest_local_ino;
    use squeezefs::meta_backend::kv::appender::AppenderIdentity;
    use squeezefs::meta_ship::manager::{ManagerClient, ManagerReply, ManagerService};
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let vol = Arc::clone(&writer.volumes[0]);
    let (host, endpoint) = holder_listener(&vol);
    let holder = vol.token_holder().unwrap().clone();
    // A wire joiner takes routing slot 100 (forest slot 101 — unleased on
    // a fresh solo mount, the rotor took 1..=64).
    let mgr = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        ManagerService::new(Arc::clone(&vol)),
    )
    .expect("manager listener");
    let mut client = ManagerClient::connect(&mgr.endpoint().to_string(), SECRET, "joiner-1", 0)
        .await
        .expect("enrollment");
    let ManagerReply::Joined { appender_id, .. } = client
        .join(
            AppenderIdentity {
                node_token: 0x5EED_0000_0000_0001,
                mount_slot: 0x1001,
                writer_id: 0xABCD_0001,
            },
            0,
        )
        .await
        .unwrap()
    else {
        panic!("join");
    };
    let routing: u16 = 100;
    assert!(matches!(
        client.acquire_slot(appender_id, routing).await.unwrap(),
        ManagerReply::SlotsGranted { .. }
    ));

    let (_reader, plane) = open_token_reader(&path, &endpoint, "reader-nh").await;
    let object = guest_local_ino(routing, 5);
    let err = match plane.serve(object, TokenWants::default()).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a foreign slot's object is never served by this manager"),
    };
    assert!(
        err.contains(&format!("appender {appender_id}")),
        "the refusal names the holder: {err}"
    );
    assert_eq!(holder.stats().not_holder_redirects, 1);
    assert_eq!(holder.stats().grants_served, 0, "nothing was granted");
    assert_eq!(holder.holders(object), 0, "nothing was registered");
    // An object of a slot THIS manager leases is served as before.
    assert!(plane
        .serve(1, TokenWants::default())
        .await
        .unwrap()
        .is_some());
    assert_eq!(holder.stats().grants_served, 1);
    plane.stop().await;
    mgr.shutdown();
    host.shutdown();
    shutdown(&writer).await;
}

/// **A failed window's rollback recalls the readers holding its records**
/// (review round 1, Issue 14). Grants re-open at the pass's settle (stage
/// A, the RAM apply); the journal write lands in stage B. Schedule: the
/// reader holds the root's dentry set; a create on the root recalls it,
/// applies, settles; the window's write FAILS (a sector fault at the ring
/// head) and the durability lane is held BEFORE its rollback — inside the
/// hold the reader re-fetches the root and sees the name the failed
/// window applied. The rollback then removes the records: it must recall
/// the undo keys' objects through the same plane FIRST, so the reader's
/// next resolve misses, re-fetches, and finds the rolled-back name GONE —
/// never a phantom record the writer's own clients never saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_windows_rollback_recalls_the_readers_holding_its_records() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let vol = Arc::clone(&writer.volumes[0]);
    let (host, endpoint) = holder_listener(&vol);
    let holder = vol.token_holder().unwrap().clone();
    Metadata::create(writer.as_ref(), 1, "pre", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-rb").await;
    assert!(plane.install_data_sink(ProbeSink::new(false)));
    assert!(Metadata::lookup(reader.as_ref(), 1, "never_landed")
        .await
        .is_err());
    assert!(plane.holds(1), "the root's dentry set is cached");

    // The next window's write fails; the lane parks before its rollback.
    let ring0 = vol.journal_ring();
    let head = ring0.core().head();
    squeezefs::uring_fs::arm_sector_write_error(ring0.physical_offset_of(head));
    let parked0 = test_conveyor_hold_parked();
    TEST_CONVEYOR_HOLD_STAGE.store(TEST_CONVEYOR_HOLD_PRE_ROLLBACK, Ordering::SeqCst);
    let w = Arc::clone(&writer);
    let create = tokio::spawn(async move {
        Metadata::create(w.as_ref(), 1, "never_landed", libc::S_IFREG | 0o644, 0, 0).await
    });
    wait_until("the lane parked before the rollback", || {
        test_conveyor_hold_parked() > parked0
    })
    .await;
    // Inside the hold: the pass recalled the root (1) and settled — the
    // reader's re-fetch serves the RAM apply, "never_landed" included.
    assert_eq!(holder.stats().recalls, 1);
    let phantom = Metadata::lookup(reader.as_ref(), 1, "never_landed").await;
    assert!(
        phantom.is_ok(),
        "the applied-but-unwritten name is what RAM serves inside the window"
    );
    assert!(plane.holds(1));

    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    let failed = create.await.unwrap();
    squeezefs::uring_fs::clear_faults();
    assert!(failed.is_err(), "the armed ring-head fault fails the write");
    // The rollback recalled the root's token before it removed the
    // records: the reader's next resolve re-fetches and the name is gone.
    wait_until("the rollback's recall reached the reader", || {
        holder.stats().recalls >= 2
    })
    .await;
    assert!(
        Metadata::lookup(reader.as_ref(), 1, "never_landed")
            .await
            .is_err(),
        "no reader holds a rolled-back record"
    );
    assert!(
        Metadata::lookup(reader.as_ref(), 1, "pre").await.is_ok(),
        "the survivors are served"
    );
    // Root at the pass; root AND the phantom child's own token (the
    // in-window lookup took it) at the rollback — every one acked.
    assert_eq!(holder.stats().recalls, 3);
    assert_eq!(
        holder.stats().recall_acks,
        holder.stats().recalls,
        "every recall was acked"
    );
    // The volume keeps working after the isolated failure.
    Metadata::create(
        writer.as_ref(),
        1,
        "after_fault",
        libc::S_IFREG | 0o644,
        0,
        0,
    )
    .await
    .unwrap();
    assert!(Metadata::lookup(reader.as_ref(), 1, "after_fault")
        .await
        .is_ok());
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// **The `..` reconnect scan fails CLOSED on a token reader** (review
/// round 1, Issue 19): `LOOKUP(dir, "..")` off the `open_by_handle_at`
/// reconnect path resolves the parent by a reverse dentry-tree scan —
/// on a token reader that scan would read the S5 projection, a
/// bounded-staleness read of user-visible metadata (R-SYM-4's second
/// method) on a cold rare path. The reader refuses `ESTALE` (the
/// reconnect path's own errno), the projection is never read, and the
/// connected `..` (the kernel's dcache) is untouched; the writer's own
/// scan serves as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dotdot_reconnect_scan_fails_closed_on_a_token_reader() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let d = Metadata::create(writer.as_ref(), 1, "d", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let sub = Metadata::create(writer.as_ref(), d, "sub", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-dotdot").await;
    let scans0 = squeezefs::fuse_client::METRICS
        .meta_parent_scans
        .load(Ordering::Relaxed);
    let err = Metadata::lookup(reader.as_ref(), sub, "..")
        .await
        .expect_err("the reconnect scan is refused on a token reader");
    assert_eq!(err.to_errno(), libc::ESTALE, "{err}");
    assert_eq!(
        squeezefs::fuse_client::METRICS
            .meta_parent_scans
            .load(Ordering::Relaxed),
        scans0,
        "the projection was never scanned"
    );
    // The writer's own reconnect scan is untouched.
    let p = Metadata::lookup(writer.as_ref(), sub, "..").await.unwrap();
    assert_eq!(p.ino, d);
    // Every other resolve on the reader serves under tokens.
    assert_eq!(
        Metadata::lookup(reader.as_ref(), d, "sub")
            .await
            .unwrap()
            .ino,
        sub
    );
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// **Grants ride a session POOL** (review round 1, Issue 16a): a grant
/// parked at the holder (its object in flight under a pass) must not
/// serialize every other grant of the volume behind it — a second
/// object's grant completes while the first is parked. Before the pool
/// every grant rode ONE mutex-guarded session, so a cold `readdir +
/// stat` of a C-creator directory was C SERIALIZED round trips and one
/// parked grant stalled them all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_grant_does_not_serialize_the_volumes_other_grants() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    let a = Metadata::create(writer.as_ref(), 1, "a", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let b = Metadata::create(writer.as_ref(), 1, "b", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-pool").await;
    assert!(
        plane.stats().grant_sessions >= 2,
        "the grant pool is at least two sessions deep: {}",
        plane.stats().grant_sessions
    );
    // A pass on `a` parked after its recall union: a grant on `a` parks.
    let parked0 = squeezefs::meta_backend::kv::backend::test_conveyor_post_recall_parked();
    TEST_CONVEYOR_HOLD_STAGE.store(
        squeezefs::meta_backend::kv::backend::TEST_CONVEYOR_HOLD_POST_RECALL,
        Ordering::SeqCst,
    );
    let w = Arc::clone(&writer);
    let create = tokio::spawn(async move {
        Metadata::create(w.as_ref(), a, "child", libc::S_IFREG | 0o644, 0, 0).await
    });
    wait_until("the pass parked in the post-recall window", || {
        squeezefs::meta_backend::kv::backend::test_conveyor_post_recall_parked() > parked0
    })
    .await;
    let r = Arc::clone(&reader);
    let grant_a = tokio::spawn(async move { Metadata::getattr(r.as_ref(), a).await });
    wait_until("the grant on `a` parked at the holder", || {
        holder.stats().grant_parks >= 1
    })
    .await;
    // `b`'s grant completes while `a`'s is parked.
    let t0 = std::time::Instant::now();
    let got_b = tokio::time::timeout(
        Duration::from_secs(5),
        Metadata::getattr(reader.as_ref(), b),
    )
    .await
    .expect("a grant on another object is not queued behind the parked one")
    .unwrap();
    assert_eq!(got_b.ino, b);
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert!(!grant_a.is_finished(), "`a`'s grant is still parked");
    TEST_CONVEYOR_HOLD_STAGE.store(0, Ordering::SeqCst);
    test_conveyor_hold_release();
    create.await.unwrap().unwrap();
    grant_a.await.unwrap().unwrap();
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// **The reader's install is conditional on the generation it read
/// under** (review round 2, Issue 21 — the reader-side half of Issue 2).
/// Schedule: the reader's fetch of B evicts A to make room — the eviction
/// runs the sink's drain (parked here); the holder had REGISTERED B's
/// grant before its read, so a commit on B recalls this reader while the
/// fetch sits inside its eviction; the recall handler bumps B's revoke
/// generation, finds nothing to remove, drains (parked on the same sink)
/// and acks; the pass applies. Before the fix the fetch re-checked the
/// generation BEFORE the eviction's await and installed the PRE-COMMIT
/// records LIVE after it — nothing recalled them until the next commit on
/// B. The law: the generation check is atomic with the install (under
/// the cache entry), a moved generation retries the fetch, and the
/// reader's next resolve sees the committed record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recall_landing_inside_the_fetchs_eviction_never_installs_pre_commit_records() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    let a = Metadata::create(writer.as_ref(), 1, "a", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let b = Metadata::create(writer.as_ref(), 1, "b", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-i21").await;
    let sink = ProbeSink::new(false);
    assert!(plane.install_data_sink(sink.clone()));
    let (_v, b_local) = writer.route_ino(b);
    let _ = Metadata::getattr(reader.as_ref(), a).await.unwrap();
    let held = plane.stats().cached_bytes;
    plane.test_set_records_budget(Some(held + held / 2));

    // B's fetch: the grant is registered and read, then the eviction of A
    // parks in the sink.
    sink.parked.store(true, Ordering::SeqCst);
    let r = Arc::clone(&reader);
    let fetch_b = tokio::spawn(async move { Metadata::getattr(r.as_ref(), b).await });
    wait_until("the eviction entered the sink", || {
        sink.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(holder.holders(b_local), 1, "B's grant is registered");
    // The conflicting commit on B recalls the reader inside the window.
    let w = Arc::clone(&writer);
    let commit = tokio::spawn(async move {
        Metadata::setattr(
            w.as_ref(),
            b,
            Some(0o600),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
    });
    wait_until(
        "the recall of B reached the reader and entered the sink",
        || plane.stats().recalls_received == 1 && sink.calls.load(Ordering::SeqCst) == 2,
    )
    .await;
    sink.release();
    commit.await.unwrap().unwrap();
    fetch_b.await.unwrap().unwrap();
    // The law: the reader's next resolve sees the committed mode — never
    // the pre-commit records the fetch read.
    let seen = Metadata::getattr(reader.as_ref(), b).await.unwrap();
    assert_eq!(
        seen.mode & 0o777,
        0o600,
        "a recall inside the fetch's eviction must not leave pre-commit records live"
    );
    assert!(plane.holds(b_local));
    assert!(
        plane.stats().fetch_retries >= 1,
        "the fetch that spanned the recall retried"
    );
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// **Attrs + xattrs ride the FIRST page and the xattrs page too**
/// (review round 1, Issue 16b): a directory whose carried xattrs alone
/// exceed one frame's payload is served — the xattrs are paged by name
/// under the same byte budget the dentries share, a continuation page
/// carries no xattrs, and every name and value arrives intact. Before
/// it the first page carried every xattr beside a full dentry page, so
/// such an object failed the holder's encode (> 1 MiB) and was
/// fail-closed EIO for ever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_objects_xattrs_are_paged_under_the_grant_budget_and_never_resent() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let d = Metadata::create(writer.as_ref(), 1, "wide", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    for i in 0..200 {
        Metadata::create(
            writer.as_ref(),
            d,
            &format!("e{i:04}"),
            libc::S_IFREG | 0o644,
            0,
            0,
        )
        .await
        .unwrap();
    }
    // 66 × 16 KiB (the 64 KiB node's value cap) ≈ 1.03 MiB of xattrs —
    // past the whole frame cap on their own.
    let value_cap = squeezefs::meta_backend::kv::node::xattr_value_cap(NODE_SIZE);
    for i in 0..66u32 {
        let v = vec![(i & 0xFF) as u8; value_cap];
        Metadata::setxattr(writer.as_ref(), d, &format!("user.x{i:03}"), &v)
            .await
            .unwrap();
    }
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-xattr").await;
    // The in-process R5 budget is 0, so the records budget sits at its 1
    // MiB floor — below this object's records; the pin is the PAGING
    // (Issue 7's byte law has its own contract), so the budget is raised.
    plane.test_set_records_budget(Some(8 * 1024 * 1024));
    let names = Metadata::listxattr(reader.as_ref(), d).await.unwrap();
    assert_eq!(
        names.iter().filter(|n| n.starts_with("user.x")).count(),
        66,
        "every xattr name arrived"
    );
    let v65 = Metadata::getxattr(reader.as_ref(), d, "user.x065")
        .await
        .unwrap()
        .expect("the last xattr");
    assert_eq!(v65, vec![65u8; value_cap]);
    let v0 = Metadata::getxattr(reader.as_ref(), d, "user.x000")
        .await
        .unwrap()
        .expect("the first xattr");
    assert_eq!(v0, vec![0u8; value_cap]);
    let page = Metadata::readdir(reader.as_ref(), d, 0, 1024)
        .await
        .unwrap();
    assert!(page.len() >= 200, "the dentry set arrived too");
    assert!(
        plane.stats().grants >= 3,
        "the object's records were paged: {} grant page(s)",
        plane.stats().grants
    );
    assert!(
        plane.stats().cached_bytes < 2 * 66 * value_cap as u64,
        "the xattrs were charged once, never per page"
    );
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// The negative contract: `SQUEEZEFS_SYMMETRIC_META=0` and a flat volume
/// carry NO token plane — no holder, no reader, the recall gate off, the
/// Token family 0 — and an unarmed forest's frames are v2 under the
/// manager's stamp with nothing screened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symmetric_meta_off_carries_no_token_plane() {
    let _g = SEAM.lock().await;
    free_grace::reset_for_test();
    let screened0 = META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed);
    let gated0 = free_grace::recall_gated_frees();
    let deferred0 = free_grace::timeout_deferrals();
    let dir = tempfile::tempdir().unwrap();
    let stamped = format_stamped(dir.path(), "meta0").await;
    let flat = dir.path().join("flat");
    std::fs::File::create(&flat)
        .unwrap()
        .set_len(VOL_LEN)
        .unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    format_v3_stamped(&flat, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .unwrap();
    std::env::remove_var(SYMMETRIC_META_ENV);
    for p in [&stamped, &flat] {
        let w = KvMetaBackend::open(p).await.unwrap();
        assert!(w.token_holder().is_none(), "{}", p.display());
        assert!(w.token_reader().is_none());
        assert_eq!(
            free_grace::recall_gate_verdict(),
            free_grace::RecallGate::Off
        );
        Metadata::create(w.as_ref(), 1, "x", libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        w.checkpoint_now().await.unwrap();
        w.shutdown().await.unwrap();
        let r = KvMetaBackend::open_read_only(p).await.unwrap();
        assert!(r.token_reader().is_none());
        assert_eq!(
            ro_coherence::metadata_staleness_bound_ms(&[Arc::clone(&r)]),
            ro_coherence::reader_staleness_bound().as_millis() as u64,
            "the S5 bound stands where no token plane exists"
        );
        let _ = Metadata::lookup(r.as_ref(), 1, "x").await.unwrap();
    }
    assert_eq!(
        META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed),
        screened0
    );
    assert_eq!(free_grace::recall_gated_frees(), gated0);
    assert_eq!(free_grace::timeout_deferrals(), deferred0);
}

/// A minimal data plane for the mount-path arm (its recall sink drains
/// the router's in-flight serves and purges its block-key census).
async fn data_router(vol_id: &str) -> (squeezefs::routing::DataRouter, tempfile::NamedTempFile) {
    let b = tempfile::NamedTempFile::new().unwrap();
    b.as_file().set_len(16 * 1024 * 1024).unwrap();
    let dev = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new(vol_id)
            .await
            .unwrap(),
    );
    let cache = squeezefs::cache::TieredCache::new(
        Vec::new(),
        Some("8MB"),
        Some("8MB"),
        Some("8MB"),
        Some("8MB"),
        ba.clone(),
        dev.clone(),
        None,
    )
    .await
    .unwrap();
    let dlm = squeezefs::dlm::DlmClient::new().unwrap();
    (squeezefs::routing::DataRouter::new(dlm, cache, ba, dev), b)
}

fn device_digest(path: &std::path::Path) -> u64 {
    xxhash_rust::xxh3::xxh3_64(&std::fs::read(path).expect("read volume image"))
}

/// The image once the live writer has gone quiet: two samples one
/// checkpoint landing ceiling apart agree (bounded) — a quiet gap
/// between two cadence cycles is shorter than that.
async fn settled_device_digest(path: &std::path::Path) -> u64 {
    let started = std::time::Instant::now();
    let mut last = device_digest(path);
    let gap = Duration::from_millis(
        squeezefs::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_derived() + 100,
    );
    loop {
        tokio::time::sleep(gap).await;
        let now = device_digest(path);
        if now == last {
            return now;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the writer never went quiet"
        );
        last = now;
    }
}

/// **§5.7.2 — the `-o ro` MOUNT PATH**: with the reader latched, the knob
/// on, a membership lease held, the holder's endpoint declared and the
/// cluster secret on the volume, `ro_coherence::arm_token_readers` (the
/// one call `fuse_client::init` makes) arms the token client on every
/// read-only volume, the first resolve is served under a GRANT, the
/// published bound reads 0 — and the reader session writes **zero bytes**
/// to the volume (S5's Item-1 pin extended to the token posture: a grant
/// is a wire call, never a device write).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ro_mount_under_the_knob_arms_the_token_client_and_writes_nothing() {
    use squeezefs::membership::{
        self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MemberSession,
        MembershipOwner,
    };
    let _g = SEAM.lock().await;
    free_grace::reset_for_test();
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let holder = &writer.volumes[0];
    let (_listener, endpoint) = holder_listener(holder);
    // The cluster secret — the wire's root of trust — as the job wire
    // writes it (`{"secret": hex}` on ino 1).
    let hex: String = SECRET.iter().map(|b| format!("{b:02x}")).collect();
    holder
        .setxattr_internal(
            1,
            squeezefs::job_wire::JOB_ENROLL_XATTR,
            format!("{{\"secret\":\"{hex}\"}}").as_bytes(),
        )
        .await
        .expect("enroll record");
    let ino = writer
        .create(1, "seen-through-a-token", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create")
        .ino;
    holder.checkpoint_now().await.expect("checkpoint");

    // The membership lease: this process holds a member-reader session
    // (the mount path's `arm_mount_membership(read_only = true)` outcome).
    let clocks = LeaseClocks::derive(Duration::ZERO).expect("shipped clocks");
    let clock = LeaseClock::monotonic();
    let owner =
        MembershipOwner::arm("owner-ro-mount", 5, 4, clocks, clock.clone()).expect("owner arms");
    let grant = match owner.join(JoinRequest {
        id: "ro-mount-reader".to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-test".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) {
        JoinOutcome::Granted(g) => g,
        other => panic!("join must grant: {other:?}"),
    };
    membership::install_member(Arc::new(MemberSession::adopt(
        "ro-mount-reader",
        MemberRole::Reader,
        &grant,
        clock.now_ms(),
        clock,
    )));
    squeezefs::fuse_client::set_read_only_mount(true);
    std::env::set_var(SYMMETRIC_META_ENV, "1");

    // Issue 17 — the arm PROBES the holder: an endpoint that serves no
    // token verbs (a listener with an empty verb router — the shape of a
    // writer whose plane is unarmed) refuses the mount naming the writer's
    // knobs, never mounts into an EIO-at-every-resolve reader.
    let dark = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        Arc::new(squeezefs::data_grant::AsyncVerbRouter::new()),
    )
    .expect("dark listener");
    std::env::set_var(
        squeezefs::cowriter::MW_AUTHORITY_ENV,
        dark.endpoint().to_string(),
    );
    {
        let probe_reader = open_routed_meta_set_read_only(&[path.display().to_string()])
            .await
            .expect("read-only open");
        let (router, _b) = data_router("ro_mount_tokens_dark").await;
        let err = ro_coherence::arm_token_readers(&probe_reader.volumes, &router)
            .await
            .expect_err("a holder that serves no tokens refuses the arm");
        assert!(
            err.contains("does not serve read tokens"),
            "the refusal names the probe's finding: {err}"
        );
        assert!(
            err.contains("SQUEEZEFS_MULTI_WRITER=1"),
            "and the writer's knobs: {err}"
        );
        shutdown(&probe_reader).await;
    }
    dark.shutdown();
    std::env::set_var(squeezefs::cowriter::MW_AUTHORITY_ENV, &endpoint);

    // Settle the WRITER's image first: a checkpoint's tail releases the
    // pending frees it covers AFTER its bitmap pages landed, so the next
    // cadence cycle writes them — the writer is quiet only once two
    // samples a cadence apart agree.
    let before = settled_device_digest(&path).await;
    let reader = open_routed_meta_set_read_only(&[path.display().to_string()])
        .await
        .expect("read-only open");
    let (router, _b) = data_router("ro_mount_tokens").await;
    let armed = ro_coherence::arm_token_readers(&reader.volumes, &router)
        .await
        .expect("the posture's inputs are all present");
    assert_eq!(armed, 1, "every read-only volume of the set arms");
    let plane = Arc::clone(reader.volumes[0].token_reader().expect("armed"));
    wait_until("the recall channel completes its first round", || {
        plane.stats().channel_fresh
    })
    .await;
    assert_eq!(
        ro_coherence::metadata_staleness_bound_ms(&reader.volumes),
        0,
        "user-visible metadata is exact under tokens"
    );
    let hit = reader
        .lookup(1, "seen-through-a-token")
        .await
        .expect("the foreign create resolves");
    assert_eq!(hit.ino, ino);
    assert_eq!(
        plane.stats().grants,
        2,
        "the first resolve is served under GRANTS — the root's dentry set, then the child's record"
    );
    reader.volumes[0].sync_device().await.expect("sync");
    assert_eq!(
        before,
        device_digest(&path),
        "a token reader's session must not write ONE byte to the volume"
    );

    shutdown(&reader).await;
    membership::uninstall();
    std::env::remove_var(squeezefs::cowriter::MW_AUTHORITY_ENV);
    std::env::remove_var(SYMMETRIC_META_ENV);
    squeezefs::fuse_client::set_read_only_mount(false);
    shutdown(&writer).await;
}
