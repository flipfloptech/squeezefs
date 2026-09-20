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
//! recall is unacked rides the ring (`free_grace_recall_timeout_deferrals`), the
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

/// **A grant that lands while the client's previous token is being
/// recalled registers AFRESH — never on the registration the ack is
/// about to retire** (symmetric PR 12b round 4 — the `sym-storm` legs'
/// stale negative: a joiner's `mkdir -p` probe re-read the root's
/// dentries while its previous root token's recall was unacked; the
/// holder found the client "already" registered and served on it; the
/// ack then RETIRED that registration, so the token the reader installed
/// was tracked by nobody — the reader answered ENOENT for its OWN
/// directory for the rest of the round, `rm -rf` removed nothing, and
/// the oracle read 36 acked files as absent).
///
/// The window: the recall reached the reader (its entry dropped) and the
/// ack is held behind an in-flight data serve; a fresh read of the object
/// arrives at the holder inside it. RED before: `holders(1) == 0` with
/// the reader holding a token, and the next commit on the object recalls
/// nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_under_a_pending_recall_registers_afresh_and_stays_recallable() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-1").await;
    let holder = writer.volumes[0].token_holder().unwrap().clone();
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
    assert_eq!(holder.holders(1), 1);

    // The recall of the reader's root token, its ack held behind a serve.
    let stamp = ro_coherence::ServeStamp::begin();
    let w = Arc::clone(&writer);
    let create = tokio::spawn(async move {
        Metadata::create(w.as_ref(), 1, "c", libc::S_IFREG | 0o644, 0, 0).await
    });
    wait_until("the recall reached the reader", || {
        plane.stats().recalls_received == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!plane.holds(1), "the entry left the cache at the recall");
    assert_eq!(plane.stats().recalls_acked, 0, "the ack is held");

    // A fresh read of the object INSIDE the window: its grant reaches the
    // holder while the client's registration is still the recalled one.
    let r = Arc::clone(&reader);
    let regrant = tokio::spawn(async move { Metadata::getattr(r.as_ref(), 1).await });
    wait_until("the grant met the pending recall at the holder", || {
        holder.stats().regrant_under_recall_waits == 1
    })
    .await;
    drop(stamp);
    create.await.unwrap().unwrap();
    regrant.await.unwrap().unwrap();
    assert_eq!(plane.stats().recalls_acked, 1);
    assert!(plane.holds(1), "the re-fetched token is installed");
    assert_eq!(
        holder.holders(1),
        1,
        "the re-granted token is TRACKED at the holder — the ack retired the old registration, \
         never the new one"
    );

    // The proof the token is recallable: the next commit on the object
    // recalls it, and the reader sees the change at its next resolve.
    let w = Arc::clone(&writer);
    let create2 = tokio::spawn(async move {
        Metadata::create(w.as_ref(), 1, "d", libc::S_IFREG | 0o644, 0, 0).await
    });
    wait_until("the second recall reached the reader", || {
        plane.stats().recalls_received == 2
    })
    .await;
    create2.await.unwrap().unwrap();
    wait_until("the second recall was acked", || {
        plane.stats().recalls_acked == 2
    })
    .await;
    let seen = Metadata::lookup(reader.as_ref(), 1, "d").await;
    assert!(
        seen.is_ok(),
        "the reader's next resolve sees the commit: {seen:?}"
    );
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
    let def0 = free_grace::recall_timeout_deferrals();
    let b2 = ba.allocate_block().await.unwrap();
    ba.free_block(b2).await.unwrap();
    assert_eq!(ba.grace_len(), 2, "a free inside the window rides the ring");
    assert_eq!(free_grace::recall_timeout_deferrals(), def0 + 1);
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

/// **A token reader lists a STRIPED directory as the K-way merge of its
/// stripes, never its raw dentries** (PR 13 — the fleet's
/// `sym-shared-dir-ls` row: a `-o ro` reader has no slot-lease plane, so
/// PR 7b's `stripes_armed` read false there and the reader listed the
/// directory's RAW tree — 64 nameless stripe directories and the
/// NUL-named markers, none of the 20,000 children — `ls -l` statted 0).
/// The striping READ paths (the map, the merge, the marker filter, the
/// `stat` fold) arm on a token reader of an armed set
/// (`striping_plane_armed`); the map's markers, each stripe's dentries and
/// each child's record come as tokens from their holder — the row's
/// "K stripe tokens + C inode tokens". Pinned: the writer stripes a
/// directory of 48 names over 4 stripes; the reader lists exactly the 48
/// user names (no marker, no stripe), resolves every child by name and
/// stats it, and `stat D` folds `nlink` over the stripes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_reader_lists_a_striped_directory_as_the_merge_of_its_stripes() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let d = Metadata::create(writer.as_ref(), 1, "shared", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap()
        .ino;
    let mut names: Vec<(String, u64)> = Vec::new();
    for i in 0..48 {
        let name = format!("entry-{i:04}");
        let ino = Metadata::create(writer.as_ref(), d, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap()
            .ino;
        names.push((name, ino));
    }
    writer.stripe_dir(d, 4).await.expect("the holder's flip");
    let map = writer
        .stripe_map(d)
        .await
        .expect("map")
        .expect("striped at the writer");
    assert_eq!(map.stripes.len(), 4);
    // Every name re-homed into its stripe before the reader looks (the
    // explicit, awaited form of the holder's background migration, so the
    // row reads the finished shape, not the migrating one).
    // The flip kicked the background migration (single-flight; an explicit
    // `migrate_dir` beside it answers 0) — wait for the flag to clear.
    let started = std::time::Instant::now();
    loop {
        let _ = writer.migrate_dir(d).await.expect("the migration");
        let m = writer.stripe_map(d).await.expect("read").expect("striped");
        if !m.migrating {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the migration did not finish"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-stripes").await;
    let listed = Metadata::readdir(reader.as_ref(), d, 0, 1024)
        .await
        .expect("the reader lists the striped directory");
    let mut listed_names: Vec<String> = listed.iter().map(|e| e.name.clone()).collect();
    listed_names.sort();
    let mut want: Vec<String> = names.iter().map(|(n, _)| n.clone()).collect();
    want.sort();
    assert_eq!(
        listed_names, want,
        "the reader lists exactly the user names — no marker, no stripe"
    );
    for (name, ino) in &names {
        let got = Metadata::lookup(reader.as_ref(), d, name)
            .await
            .unwrap_or_else(|e| panic!("the reader resolves {name} through its stripe: {e}"));
        assert_eq!(got.ino, *ino, "{name}");
        Metadata::getattr(reader.as_ref(), *ino)
            .await
            .unwrap_or_else(|e| panic!("the reader stats {name}: {e}"));
    }
    let attrs = Metadata::getattr(reader.as_ref(), d)
        .await
        .expect("stat D at the reader");
    assert_eq!(
        attrs.nlink, 2,
        "a directory of files folds to nlink 2 over its stripes"
    );
    assert!(
        plane.stats().grants >= 4 + 48,
        "K stripe tokens + C inode tokens at least: {}",
        plane.stats().grants
    );
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// **A token reader that CACHED a directory's token before the flip lists
/// the merge after it — the ROOT directory included** (PR 13, the fleet's
/// `sym-crash` leg after defects 15/16: seven joiners' post-failover
/// `mkdir /after-failover-*` striped the ROOT at the successor while the
/// reader held root's token; the reader then listed `/` as EMPTY for the
/// rest of the round — every post-failover name unreadable, no error).
/// The fleet's shape, in one process: the reader reads the directory
/// (its token cached), the holder flips it to K stripes and migrates
/// every name, and the reader's next `readdir` / `lookup` must serve the
/// K-way merge exactly — the flip's marker inserts RECALLED the token,
/// the re-fetch carries the markers, the map is read off them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_reader_holding_a_directorys_token_across_its_flip_lists_the_merge() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    // The ROOT is the directory (the fleet's object): names created
    // BEFORE the reader looks, then more after the flip.
    let mut names: Vec<(String, u64)> = Vec::new();
    for i in 0..12 {
        let name = format!("pre-{i:03}");
        let ino = Metadata::create(writer.as_ref(), 1, &name, libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        names.push((name, ino));
    }
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-root-flip").await;
    let before = Metadata::readdir(reader.as_ref(), 1, 0, 1024)
        .await
        .expect("the reader lists the unstriped root");
    assert_eq!(before.len(), 12, "the pre-flip names, off root's token");
    assert!(plane.holds(1), "root's token is cached at the reader");
    let recalls0 = plane.stats().recalls_received;

    // The holder FLIPS root under the reader's token and migrates every
    // name; then names land after the flip (the fleet's post-failover
    // mkdirs, routed to their stripes).
    writer
        .stripe_dir(1, 4)
        .await
        .expect("the holder's flip of root");
    let started = std::time::Instant::now();
    loop {
        let _ = writer.migrate_dir(1).await.expect("the migration");
        let m = writer.stripe_map(1).await.expect("read").expect("striped");
        if !m.migrating {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the migration did not finish"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for i in 0..7 {
        let name = format!("after-flip-{i}");
        let ino = Metadata::create(writer.as_ref(), 1, &name, libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        names.push((name, ino));
    }
    wait_until("the flip's inserts recalled root's token", || {
        plane.stats().recalls_received > recalls0
    })
    .await;

    // The reader's next listing is the MERGE: every name, no marker, no
    // stripe — and every name resolves by lookup.
    let listed = Metadata::readdir(reader.as_ref(), 1, 0, 1024)
        .await
        .expect("the reader lists the striped root");
    let mut listed_names: Vec<String> = listed.iter().map(|e| e.name.clone()).collect();
    listed_names.sort();
    let mut want: Vec<String> = names.iter().map(|(n, _)| n.clone()).collect();
    want.sort();
    assert_eq!(
        listed_names, want,
        "the reader lists the root's merge after a flip it held a token across"
    );
    for (name, ino) in &names {
        let got = Metadata::lookup(reader.as_ref(), 1, name)
            .await
            .unwrap_or_else(|e| panic!("the reader resolves {name} through its stripe: {e}"));
        assert_eq!(got.ino, *ino, "{name}");
    }
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// **A single-flight fetch LOSER never loses its wake** (PR 13 — found by
/// the fleet's `sym-walls` row: a joiner's `lookup(1)` parked 455 s past
/// the entry station while a fresh lookup of the same name served at
/// once — a LONE lost wake). `TokenReaderPlane::fetch` is single-flight
/// per object: a second fetcher of an object in flight parks on the
/// winner's `Notify` and re-reads the cache when woken. The loser read
/// the in-flight entry, dropped it, THEN registered — and a winner that
/// finished in between removed its entry and bumped the epoch before the
/// registration: a `notified()` created after the bump is never woken
/// (`sqz_notify` registers at creation; a bump before it is not a
/// permit), and the tick heals only the epoch-gated future, never the
/// caller's outer condition — the op parked for ever. The law: register
/// FIRST, then re-check the entry is still the winner's; gone ⇒ no
/// await. Pinned through the seam that parks the loser in exactly that
/// window until the winner has finished: RED = the loser's serve never
/// returns (bounded here at 5 s), GREEN = it serves off the cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_flight_fetch_loser_registers_before_it_rechecks_the_winner() {
    use squeezefs::meta_ship::token_plane::{
        test_fetch_loser_release, TEST_FETCH_LOSER_HOLD, TEST_FETCH_LOSER_PARKED,
    };
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            test_fetch_loser_release();
        }
    }
    let _g = SEAM.lock().await;
    let _cleanup = Cleanup;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let f = Metadata::create(writer.as_ref(), 1, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let (_v, local) = writer.route_ino(f);
    let (_reader, plane) = open_token_reader(&path, &endpoint, "reader-single-flight").await;
    let parked0 = TEST_FETCH_LOSER_PARKED.load(Ordering::Relaxed);
    TEST_FETCH_LOSER_HOLD.store(true, Ordering::Release);
    // Two fetchers of one cold object: the first wins the single flight
    // and fetches; the second reads the in-flight entry and parks at the
    // seam — BEFORE its registration.
    let p1 = Arc::clone(&plane);
    let winner = tokio::spawn(async move { p1.serve(local, TokenWants::default()).await });
    let p2 = Arc::clone(&plane);
    let loser = tokio::spawn(async move { p2.serve(local, TokenWants::default()).await });
    wait_until("the loser parked at the seam", || {
        TEST_FETCH_LOSER_PARKED.load(Ordering::Relaxed) > parked0
    })
    .await;
    // The winner finishes: its entry removed, its waiters woken — the
    // loser is not among them yet.
    let served = winner.await.unwrap().expect("the winner serves");
    assert!(served.is_some());
    assert!(plane.holds(local));
    // Release the loser: it registers now, re-checks, and must SERVE.
    test_fetch_loser_release();
    let out = tokio::time::timeout(Duration::from_secs(5), loser)
        .await
        .expect("the loser's serve returned (a lost wake parks it for ever)")
        .unwrap()
        .expect("the loser serves");
    assert!(out.is_some(), "served off the winner's cache");
    assert_eq!(
        plane.stats().grants,
        1,
        "one grant — the loser re-read the cache"
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

/// **A re-opened writer's pre-arm frames carry the arm's generation**
/// (found at the rebase onto PR 6/8 — `sym_cross_owner_tests`, ten
/// contracts red, both layouts). The open's bring-up cover and the join's
/// cycle flush the D0 claim's leaf BEFORE the arm; the first build
/// stamped those frames with the manager's structural `(0, 0)` — below
/// the leased generation the leaf's earlier frames carry — and rule 3
/// screened the frame at the next load, ENDING the log there: every
/// commit of the re-opened writer landed behind it read EMPTY at the
/// remount (the writer saw them in RAM; a reader, fsck and the next open
/// did not). Now the fence and tree 0's lease table are primed before the
/// cover and the native slot's pre-arm stamp is its arm generation
/// (`Unleased { g }` → `g + 1`, the acquire's law), so the log stays
/// monotone across every open: three armed opens, each committing and
/// leaving cleanly — every name of every incarnation resolves at a
/// read-only remount and nothing was ever screened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commits_after_a_reopened_writers_join_survive_the_next_remount_unscreened() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let screened0 = META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed);
    let mut expected = Vec::new();
    for incarnation in 0..3 {
        let writer = open_armed_writer(&path).await;
        for i in 0..4 {
            let name = format!("gen{incarnation}-{i}");
            let ino = Metadata::create(writer.as_ref(), 1, &name, libc::S_IFREG | 0o644, 0, 0)
                .await
                .unwrap()
                .ino;
            expected.push((name, ino));
        }
        // Every earlier incarnation's names are still served by this one.
        for (name, ino) in &expected {
            assert_eq!(
                Metadata::lookup(writer.as_ref(), 1, name)
                    .await
                    .unwrap()
                    .ino,
                *ino,
                "{name} in incarnation {incarnation}"
            );
        }
        shutdown(&writer).await;
    }
    let reader = open_routed_meta_set_read_only(&[path.display().to_string()])
        .await
        .expect("read-only remount");
    for (name, ino) in &expected {
        assert_eq!(
            Metadata::lookup(reader.as_ref(), 1, name)
                .await
                .unwrap_or_else(|e| panic!("{name} lost across the remounts: {e}"))
                .ino,
            *ino
        );
    }
    assert_eq!(
        META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed),
        screened0,
        "nothing a legitimate writer wrote was screened"
    );
    // The C9 oracle over the same shape is `sym_cross_owner_tests`'
    // `fsck_clean` (its fixture carries the format config fsck needs).
}

/// **The once-armed volume takes no writer without the plane** (review
/// round 3, Issue 25 — the frame-stamp class of the pin above on the
/// OTHER legal transition). An armed session leaves its native-slot
/// leaves with frames at `g ≥ 1` and generation `g`'s recorded tails; a
/// `SQUEEZEFS_SYMMETRIC_META=0` WRITER session on the same bit-17 volume
/// has no lease plane and no legitimate stamp for the leaves it appends
/// to (its first commit is the `writer_claim` on ino 1's leaf): `(0, 0)`
/// is rule 3's non-monotone `g` on itself and rule 2's zombie shape past
/// the recorded tail on the next armed session — its acked commits gone
/// at the next load, silently. The law ("a slot's generations are ONE
/// sequence owned by its lease") has no unarmed writer, so the `=0`
/// writable open of a once-armed volume REFUSES loud, naming the knob,
/// before anything is written; readers and probes open as before; a
/// never-armed stamped volume (every slot at `g = 0` — the PR 1–3 shape)
/// opens `=0` exactly as shipped. The `Ok` arm below is the defect's own
/// shape — RED on the tree this lands on by the LOSS it demands be absent
/// — and the fix takes the `Err` arm.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_once_armed_volume_refuses_a_writer_without_the_plane_and_loses_nothing() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let uris = vec![path.display().to_string()];
    let screened0 = META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed);
    // A never-armed stamped volume opens `=0` writable: the shipped PR
    // 1–3 posture, untouched by the gate.
    std::env::remove_var(SYMMETRIC_META_ENV);
    let fresh = open_routed_meta_set(&uris)
        .await
        .expect("=0 on a never-armed forest");
    assert!(fresh.volumes[0].slot_lease_stats().is_none());
    shutdown(&fresh).await;

    let mut expected = Vec::new();
    let writer = open_armed_writer(&path).await;
    for i in 0..4 {
        let name = format!("armed-{i}");
        let ino = Metadata::create(writer.as_ref(), 1, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap()
            .ino;
        expected.push((name, ino));
    }
    shutdown(&writer).await;

    // The `=0` WRITER on the once-armed volume — bracketed by the device
    // image's digest (sector 0, the ledger, every node): a REFUSED open
    // writes NOTHING (review round 4, Issue 30).
    std::env::remove_var(SYMMETRIC_META_ENV);
    let image_before = device_digest(&path);
    match open_routed_meta_set(&uris).await {
        Err(e) => {
            let msg = e.to_string();
            assert!(msg.contains("SQUEEZEFS_SYMMETRIC_META=1"), "{msg}");
            assert!(msg.contains("generation"), "{msg}");
            assert_eq!(
                device_digest(&path),
                image_before,
                "the refused open left the image byte-identical"
            );
        }
        Ok(unarmed) => {
            // The defect's shape: the open succeeded, so its commits must
            // survive every later mount — they do not on the tree this
            // pin lands on.
            for i in 0..4 {
                let name = format!("unarmed-{i}");
                let ino = Metadata::create(unarmed.as_ref(), 1, &name, libc::S_IFREG | 0o644, 0, 0)
                    .await
                    .unwrap()
                    .ino;
                expected.push((name, ino));
            }
            shutdown(&unarmed).await;
        }
    }
    // Every acked record of every session is present at the next armed
    // open and at a read-only open, and nothing was screened.
    let armed = open_armed_writer(&path).await;
    for (name, ino) in &expected {
        assert_eq!(
            Metadata::lookup(armed.as_ref(), 1, name)
                .await
                .unwrap_or_else(|e| panic!("{name} lost at the armed remount: {e}"))
                .ino,
            *ino
        );
    }
    shutdown(&armed).await;
    std::env::remove_var(SYMMETRIC_META_ENV);
    let reader = open_routed_meta_set_read_only(&uris)
        .await
        .expect("a read-only open needs no plane");
    for (name, ino) in &expected {
        assert_eq!(
            Metadata::lookup(reader.as_ref(), 1, name)
                .await
                .unwrap_or_else(|e| panic!("{name} lost at the read-only remount: {e}"))
                .ino,
            *ino
        );
    }
    shutdown(&reader).await;
    let probe = KvMetaBackend::open_probe(&path)
        .await
        .expect("a probe needs no plane");
    assert!(probe.slot_lease_stats().is_none());
    drop(probe);
    assert_eq!(
        META_KV_FOREIGN_FRAMES_SCREENED.load(Ordering::Relaxed),
        screened0,
        "nothing a legitimate writer wrote was screened"
    );
}

/// **Rebase seam (a) onto PR 8 — the recall completes before the
/// terminal free reaches the allocation HOLDER.** PR 8 re-homed the free
/// ladder to the data volume's allocation-lease holder (the bitmap IS the
/// free list; a terminal free CLEARS the block's bit at `finish_free`),
/// and the recall gate sits in that same `finish_free` beside the grace
/// ring. The order is structural: the ladder (a local free, or a shipped
/// one) is issued from the displacing publish's post-commit tail, and the
/// commit is the conveyor pass that RECALLED the object's readers before
/// its apply — so no free of a displaced block reaches the holder before
/// the readers' acks. On a 2-volume armed set with the lease homed on the
/// slot-0 volume: the block's bit stays SET while the recall is parked
/// (no free ran), and after the ack the free publishes DIRECTLY under the
/// gate (`free_grace_recall_gated_frees` +1, no ring deferral) and the
/// bit CLEARS at the holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_displacing_publishs_recall_completes_before_the_holders_free_clears_the_bit() {
    use squeezefs::meta_backend::kv::alloc_lease;
    let _g = SEAM.lock().await;
    const DATA_BLOCKS: u64 = 4096;
    const DATA_TAG: u64 = 0xD0DA_0000_0000_0005;
    const DATA_ID: &str = "vol-d0da000000000005";
    alloc_lease::test_clear_holdings();
    alloc_lease::register_data_volume_blocks(DATA_TAG, DATA_BLOCKS);
    free_grace::reset_for_test();
    let dir = tempfile::tempdir().unwrap();
    let plan = plan_meta_slot_set(2).expect("derived plan");
    let mut uris = Vec::new();
    for (i, stamp) in plan.stamps.iter().enumerate() {
        let p = dir.path().join(format!("meta{i}"));
        std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
        let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), stamp.clone()).await;
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
        r.expect("format stamped member");
        uris.push(p.display().to_string());
    }
    std::env::set_var(SYMMETRIC_META_ENV, "1");
    std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
    let writer = open_routed_meta_set(&uris)
        .await
        .expect("armed 2-volume writer");
    std::env::remove_var(SYMMETRIC_META_ENV);
    std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
    let (slot0, _) = writer.route_ino(1);
    let vol0 = Arc::clone(&writer.volumes[slot0]);
    assert!(vol0.token_holder().is_some());
    // The data volume's allocator, minting from the holder's grants.
    let ba = Arc::new(
        squeezefs::block_allocator::BlockAllocator::new(DATA_ID)
            .await
            .expect("allocator"),
    );
    ba.set_capacity_bytes(DATA_BLOCKS * ba.chunk_size());
    assert_eq!(
        alloc_lease::arm_symmetric_allocation(&writer, &[Arc::clone(&ba)])
            .await
            .unwrap(),
        1
    );
    let holding = alloc_lease::holding(DATA_TAG).expect("the lease is held on the slot-0 volume");
    let b_off = ba.allocate_block().await.unwrap();
    let b = b_off / ba.chunk_size();
    assert!(holding.bitmap.is_set(b), "a granted block's bit is SET");

    let x = Metadata::create(writer.as_ref(), 1, "x", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    // Every volume's holder on its own listener; the reader's plane per
    // volume (the mint policy homes `x` wherever the rotor says — the
    // recall rides the plane of its home).
    let reader = open_routed_meta_set_read_only(&uris)
        .await
        .expect("read-only 2-volume open");
    let mut hosts = Vec::new();
    let mut planes = Vec::new();
    let sink = ProbeSink::new(true);
    for (i, v) in writer.volumes.iter().enumerate() {
        let (host, endpoint) = holder_listener(v);
        let plane = reader.volumes[i]
            .arm_token_reader(TokenClientConfig {
                endpoint,
                secret: SECRET.to_vec(),
                client_id: "reader-seam-a".to_string(),
                volume: i as u16,
            })
            .expect("token client arms");
        wait_until("the recall channel completes its first round", || {
            plane.stats().channel_fresh
        })
        .await;
        assert!(plane.install_data_sink(sink.clone()));
        hosts.push(host);
        planes.push(plane);
    }
    let (x_vol, _) = writer.route_ino(x);
    let plane = Arc::clone(&planes[x_vol]);
    let _ = Metadata::getattr(reader.as_ref(), x).await.unwrap();
    let gated0 = free_grace::recall_gated_frees();
    let deferrals0 = free_grace::deferrals();

    // The displacing publish on X: its recall parks at the reader.
    let w = Arc::clone(&writer);
    let publish = tokio::spawn(async move {
        Metadata::setattr(
            w.as_ref(),
            x,
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
    wait_until("the recall entered the reader's sink", || {
        sink.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!publish.is_finished(), "the commit waits on the ack");
    assert!(
        holding.bitmap.is_set(b),
        "no free of the displaced block ran before the recall completed"
    );
    assert_eq!(free_grace::recall_gated_frees(), gated0);
    sink.release();
    publish.await.unwrap().unwrap();
    // The commit returned on the holder OBSERVING the ack; the reader's
    // own `recalls_acked` word moves after its reply left, so the pin
    // waits for it (PR 12 — the test-side race PR 5's note named).
    wait_until("the reader counted its ack", || {
        plane.stats().recalls_acked == 1
    })
    .await;
    // The publish's ladder: the displaced block's terminal free at the
    // holder — direct under the recall gate, its bit cleared, no ring.
    assert!(ba.begin_free(b_off));
    ba.finish_free(b_off);
    assert!(
        !holding.bitmap.is_set(b),
        "the holder cleared the bit at finish_free"
    );
    assert_eq!(
        free_grace::recall_gated_frees(),
        gated0 + 1,
        "published directly"
    );
    assert_eq!(free_grace::deferrals(), deferrals0, "no ring deferral");
    for p in &planes {
        p.stop().await;
    }
    for h in &hosts {
        h.shutdown();
    }
    shutdown(&writer).await;
    alloc_lease::test_clear_holdings();
}

/// **Rebase seam (c) onto PR 8 — a token READER's `T_self` is the
/// POISON, never the park.** PR 8 split the member's fence: a symmetric
/// APPENDER (a `Writer` member on a mount that armed the appender region)
/// PARKS at `T_self` and reclaims against the successor; every other
/// lease keeps the shipped poison. A token reader is a `Reader` member
/// holding no region — even in a process where the appender posture IS
/// armed, `self_fence_as(SymmetricAppender)` falls through to the poison
/// (`fenced()`, purge requested, nothing parked), and the token plane's
/// lease gate reads the poison: the reader drops its tokens and serves
/// NOTHING from cache (`dlm_token_serve_refusals`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_readers_t_self_poisons_and_never_parks_under_the_appender_posture() {
    use squeezefs::membership::{
        self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MemberSession,
        MembershipOwner,
    };
    use squeezefs::park_gate::{self, FenceClass};
    let _g = SEAM.lock().await;
    membership::uninstall();
    park_gate::test_reset();
    let ticks = Arc::new(AtomicU64::new(50_000));
    let clock = LeaseClock::manual(Arc::clone(&ticks));
    let owner = MembershipOwner::arm(
        "poison-owner",
        3,
        2,
        LeaseClocks::derive(Duration::from_micros(250)).expect("the shipped derivation"),
        clock.clone(),
    )
    .expect("arm the owner");
    let JoinOutcome::Granted(grant) = owner.join(JoinRequest {
        id: "reader-poison".to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-poison".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) else {
        panic!("join");
    };
    let session = Arc::new(MemberSession::adopt(
        "reader-poison",
        MemberRole::Reader,
        &grant,
        clock.now_ms(),
        clock.clone(),
    ));
    membership::install_member(Arc::clone(&session));
    // The appender posture ARMED in this process (as a co-located armed
    // writer would arm it): the park class exists, for WRITER members.
    park_gate::arm_symmetric_appender(0, 60_000);
    assert!(park_gate::symmetric_appender_armed());

    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let (reader, plane) = open_token_reader(&path, &endpoint, "reader-poison").await;
    let _ = Metadata::getattr(reader.as_ref(), 1).await.unwrap();
    assert!(plane.holds(1));

    // Past T_self on the member's own clock: the renewal tick's fence.
    ticks.fetch_add(grant.t_owner_ms + 1, Ordering::SeqCst);
    assert!(session.self_fence_due());
    let fence = session.self_fence_as(FenceClass::SymmetricAppender, "T_self in the contract");
    assert!(
        !fence.parked,
        "a Reader member never parks — it holds no region"
    );
    assert!(fence.purge_requested, "the reader is told to purge");
    assert!(session.fenced(), "the poison is terminal");
    assert!(!park_gate::is_parked(), "no park was raised");
    // The token plane reads the poison: nothing served from cache.
    let refused = plane.serve(1, TokenWants::default()).await;
    assert!(refused.is_err(), "a fenced reader serves nothing");
    assert!(!plane.holds(1), "its tokens were dropped");
    assert!(plane.stats().serve_refusals >= 1);
    membership::uninstall();

    // The HOLDER's side of PR 8's park (`park_gate::admits_token_service`,
    // the stand-in PR 8 left for this plane): a parked lessee still grants
    // and recalls; a park that EXPIRED poisoned custody, and every token
    // verb refuses — the successor owns the slots.
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    plane.test_set_lease_live(Some(true));
    assert!(plane
        .serve(1, TokenWants::default())
        .await
        .unwrap()
        .is_some());
    // The appender's own T_self: the PARK — grants continue through it.
    assert_eq!(
        park_gate::fence_at_t_self(
            FenceClass::SymmetricAppender,
            clock.now_ms(),
            "the contract's park"
        ),
        park_gate::TSelfAction::Parked
    );
    assert!(park_gate::is_parked() && park_gate::admits_token_service());
    let reader_p = open_routed_meta_set_read_only(&[path.display().to_string()])
        .await
        .expect("read-only open while the holder is parked");
    let plane_p = reader_p.volumes[0]
        .arm_token_reader(TokenClientConfig {
            endpoint: endpoint.clone(),
            secret: SECRET.to_vec(),
            client_id: "reader-while-parked".to_string(),
            volume: 0,
        })
        .expect("token client arms");
    wait_until("the parked holder still answers the channel", || {
        plane_p.stats().channel_fresh
    })
    .await;
    assert!(
        plane_p
            .serve(1, TokenWants::default())
            .await
            .unwrap()
            .is_some(),
        "a PARKED lessee is still the lock master: it grants"
    );
    // The park EXPIRES (the reclaim answered "not custody"): every verb
    // refuses from here.
    assert!(park_gate::expire_now("the contract's expiry"));
    assert!(!park_gate::admits_token_service());
    // A reader arming against the expired holder: its channel poll is
    // REFUSED (the channel never becomes fresh), and every serve fails
    // closed; the already-fresh parked reader's next grant is refused too.
    let reader2 = open_routed_meta_set_read_only(&[path.display().to_string()])
        .await
        .expect("read-only open");
    let plane2 = reader2.volumes[0]
        .arm_token_reader(TokenClientConfig {
            endpoint: endpoint.clone(),
            secret: SECRET.to_vec(),
            client_id: "reader-after-expiry".to_string(),
            volume: 0,
        })
        .expect("token client arms");
    wait_until("the expired holder refused the channel poll", || {
        holder.stats().park_expired_refusals >= 1
    })
    .await;
    assert!(
        !plane2.stats().channel_fresh,
        "no round completes against an expired holder"
    );
    assert!(
        plane2.serve(1, TokenWants::default()).await.is_err(),
        "an expired holder grants nothing — the reader fails closed"
    );
    let err = match plane_p.serve(2, TokenWants::default()).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a grant off an expired holder"),
    };
    assert!(
        err.contains("EXPIRED") || err.contains("not fresh"),
        "the refusal names the expiry or the dead channel: {err}"
    );
    drop(reader2);
    drop(reader_p);
    park_gate::test_reset();
    squeezefs::data_custody::test_clear_poison();
    plane2.stop().await;
    plane_p.stop().await;
    plane.stop().await;
    host.shutdown();
    shutdown(&writer).await;
}

/// **Rebase seam (e) onto PR 6 — a shipped cross-owner STEP is recalled
/// before it applies.** PR 6's travelling guards are 4a guards the
/// initiator holds AT THE HOLDER for the op; the step it ships applies
/// at the holder as an ordinary commit — through the conveyor pass, whose
/// FIRST act under the batch's guards is the token recall of the batch's
/// objects. So a create in a directory another appender holds recalls
/// the directory's token from every reader BEFORE the `InsertDentry`
/// step lands (the guards travelled first, the recall runs inside the
/// step's pass, the apply follows the acks), and the reader's next lookup
/// sees the child — exact. PR 6's own two-holder fixture: a directory
/// seeded in slot 4, the next open's declared region 1 leasing it, the
/// S8 venue standing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cross_owner_create_recalls_the_parents_token_before_its_shipped_step_applies() {
    use squeezefs::data_grant::AsyncVerbRouter;
    use squeezefs::meta_backend::crossvol_tx::{
        cross_owner_stats, install_xv_shipper, uninstall_xv_shipper,
    };
    use squeezefs::meta_backend::kv::appender::TEST_APPENDER_SLOTS_ENV;
    use squeezefs::meta_backend::kv::builder::ROOT_INO;
    use squeezefs::meta_backend::kv::record::ForestSlot;
    use squeezefs::meta_backend::{make_global_ino_width, IntentCreatePreset};
    use squeezefs::meta_ship::manager::ManagerSetService;
    use squeezefs::meta_ship::{MetaShipRouter, MetaShipService};
    const SLOT_B: ForestSlot = 4;
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();
    // A data volume in the format config (the cross-owner create's mint).
    let oss = dir.path().join("oss0");
    std::fs::File::create(&oss)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let cfg = squeezefs::FormatConfig {
        name: "squeezefs".to_string(),
        block_size: 4096,
        capacity: 1 << 30,
        inodes: 1_000_000,
        compression: "none".to_string(),
        encrypt_algo: "none".to_string(),
        encrypt_key: None,
        encrypt_key_ref: None,
        mem_cache_size: None,
        disk_cache_size: None,
        disk_cache_paths: None,
        data_lv: Some(vec![oss.display().to_string()]),
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
    };
    let p = dir.path().join("meta0");
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    let opts = FormatV3Options {
        format_config_xattr: Some(serde_json::to_vec(&cfg).unwrap()),
        ..set_opts()
    };
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let r = format_v3_stamped(&p, VOL_LEN, &opts, plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format stamped member");
    let uris = vec![p.display().to_string()];

    // Seed a directory in slot 4 while the slot is the manager's, then
    // release the slot so the next open's declared region takes it.
    let writer0 = open_armed_writer(&p).await;
    let shared = {
        let vol = &writer0.volumes[0];
        let width = writer0.routing_width();
        let routing = u64::from(SLOT_B) - 1;
        let local = vol
            .allocate_guest_ino(routing as u16)
            .expect("a guest cursor");
        let global = make_global_ino_width(local, routing, width);
        let ino = writer0
            .create_with_rdev_preset(
                ROOT_INO,
                "shared",
                libc::S_IFDIR | 0o755,
                0,
                0,
                0,
                0,
                Some(IntentCreatePreset {
                    global_ino: global,
                    ts_ns: KvMetaBackend::now_ns_pub(),
                }),
            )
            .await
            .expect("seed dir")
            .ino;
        assert_eq!(ino, global);
        ino
    };
    writer0.volumes[0]
        .release_slot_handover(0, SLOT_B)
        .await
        .expect("release to unleased");
    shutdown(&writer0).await;
    drop(writer0);

    // The two-holder open: region 1 leases slot 4.
    std::env::set_var(SYMMETRIC_META_ENV, "1");
    std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
    std::env::set_var(TEST_APPENDER_SLOTS_ENV, "1:4");
    let mut opened = None;
    for _ in 0..200 {
        match open_routed_meta_set(&uris).await {
            Ok(r) => {
                opened = Some(r);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    std::env::remove_var(SYMMETRIC_META_ENV);
    std::env::remove_var("SQUEEZEFS_SYM_ALLOW_NON_PR");
    std::env::remove_var(TEST_APPENDER_SLOTS_ENV);
    let writer = opened.expect("the two-holder open");
    let vol = Arc::clone(&writer.volumes[0]);
    let lease_plane = vol.slot_leases().expect("armed");
    assert_eq!(
        lease_plane.holders.holder(SLOT_B).map(|h| h.appender_id),
        Some(1),
        "the declared region leases the shared directory's slot"
    );
    // The S8 venue: the holders' owner service + the initiator's shipper.
    let venue = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        Arc::new(
            AsyncVerbRouter::new()
                .with_meta(MetaShipService::new(Arc::clone(&writer)))
                .with_manager(ManagerSetService::new(&writer.volumes)),
        ) as Arc<dyn cw::RpcAsyncService>,
    )
    .expect("owner listener");
    lease_plane
        .holders
        .set_endpoint(1, &venue.endpoint().to_string());
    install_xv_shipper(MetaShipRouter::new(
        Arc::clone(&writer),
        "node-b",
        SECRET.to_vec(),
    ));

    // The token reader holds the shared directory's dentry set.
    let (host, endpoint) = holder_listener(&vol);
    let holder = vol.token_holder().unwrap().clone();
    let (reader, manager_plane) = open_token_reader(&p, &endpoint, "reader-xv").await;
    let sink = ProbeSink::new(false);
    assert!(manager_plane.install_data_sink(sink.clone()));
    // PR 12 — the reader's per-slot binding: the shared directory lives
    // in the DECLARED region's slot, so its token is granted by holder 1's
    // plane (dialed on the bound endpoint — this process's listener, which
    // serves the declared region too), never the manager's.
    reader.volumes[0].bind_reader_holder_endpoint(1, &endpoint);
    let (_v, shared_local) = writer.route_ino(shared);
    let plane = reader.volumes[0]
        .token_reader_for(shared_local)
        .await
        .expect("resolves through tree 0")
        .expect("a token reader");
    // One plane per LISTENER: holder 1 is served by the manager's daemon
    // here, so its objects ride the manager's plane and its ONE recall
    // channel (a second channel under one client id would take the
    // recall the other plane's cache needs).
    assert!(
        Arc::ptr_eq(&plane, &manager_plane),
        "one listener, one plane"
    );
    assert!(Metadata::lookup(reader.as_ref(), shared, "out.bin")
        .await
        .is_err());
    assert!(plane.holds(shared_local), "the directory's token is cached");
    let steps0 = cross_owner_stats().steps_served;

    // The cross-owner create: its InsertDentry step lands at the holder
    // through the conveyor pass — recalled BEFORE it applies.
    sink.parked.store(true, Ordering::SeqCst);
    let w = Arc::clone(&writer);
    let create = tokio::spawn(async move {
        w.create(shared, "out.bin", libc::S_IFREG | 0o644, 0, 0)
            .await
    });
    wait_until("the step's recall reached the reader", || {
        sink.calls.load(Ordering::SeqCst) >= 1
    })
    .await;
    assert!(
        !plane.holds(shared_local),
        "the directory's token left the cache"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !create.is_finished(),
        "the shipped step cannot apply before the reader's ack"
    );
    assert_eq!(holder.stats().recall_acks, 0);
    sink.release();
    let file = create.await.unwrap().expect("the cross-owner create");
    assert_eq!(
        cross_owner_stats().steps_served - steps0,
        1,
        "one shipped InsertDentry"
    );
    assert!(holder.stats().recall_acks >= 1);
    // Exact at the next resolve.
    let seen = Metadata::lookup(reader.as_ref(), shared, "out.bin")
        .await
        .unwrap();
    assert_eq!(seen.ino, file.ino);

    plane.stop().await;
    host.shutdown();
    uninstall_xv_shipper();
    venue.shutdown();
    shutdown(&writer).await;
}

/// **The gate's in-flight set is a REFCOUNT** (review round 2, Issue
/// 23): the plane has two users that overlap by design — the conveyor
/// pass task and the durability lane's rollback of a failed window — and
/// when both hold one object in flight, the FIRST settle must not clear
/// the other's mark (a grant registered between would proceed against
/// the rollback in progress). The object leaves the flight with its LAST
/// user; a grant registered under either user's window parks.
#[test]
fn the_grant_pass_gate_keeps_an_object_in_flight_until_its_last_user_settles() {
    use squeezefs::token_grant_core::{GrantAdmission, GrantPassGate, HolderTable};
    use std::collections::{BTreeMap, BTreeSet};
    struct Table(std::sync::Mutex<BTreeMap<u64, BTreeSet<String>>>);
    impl HolderTable for Table {
        fn register(&self, object: u64, client: &str) -> bool {
            self.0
                .lock()
                .unwrap()
                .entry(object)
                .or_default()
                .insert(client.to_string())
        }
        fn holders(&self, object: u64) -> usize {
            self.0.lock().unwrap().get(&object).map_or(0, |s| s.len())
        }
    }
    let gate = GrantPassGate::new();
    let table = Table(std::sync::Mutex::new(BTreeMap::new()));
    const X: u64 = 77;
    // Two users mark X (the rollback's undo key and a later pass's key of
    // the same ino).
    gate.pass_begin(&[X], &table);
    gate.pass_begin(&[X, 78], &table);
    assert!(gate.is_inflight(X));
    // The first user settles: X stays in flight for the second.
    assert!(!gate.settle(&[X]), "the first settle clears no one's mark");
    assert!(
        gate.is_inflight(X),
        "the second user still holds X in flight"
    );
    let (already, admission) = gate.grant_register(X, "r", &table);
    assert!(!already);
    assert_eq!(
        admission,
        GrantAdmission::Park,
        "a grant parks under the second user's window"
    );
    // The last user settles: X leaves the flight (the parked grants wake).
    assert!(gate.settle(&[X, 78]));
    assert!(!gate.is_inflight(X));
    assert_eq!(gate.inflight_len(), 0);
    // A settle by nobody's window is inert.
    assert!(!gate.settle(&[X]));
    assert_eq!(
        gate.grant_register(X, "r2", &table).1,
        GrantAdmission::Proceed
    );
}

/// **A dead reader's grants die at the owner's EVICTION, never at the
/// recall deadline** (review round 2, Issue 5's residual). The REAL S6
/// owner on a manual clock, installed for the process; the reader joins
/// as a `Reader` member and takes 64 tokens; then it is KILLED (no ack
/// will ever travel). Arm (a): a pass recalling one of its tokens parks;
/// the owner's cadence sweep (`expire_due`) EVICTS the dead reader — the
/// member is REMOVED, so its lease reads `None` — and the pass must
/// complete AT the eviction (the holder is told at the eviction instant
/// and the verdict for a token client the owner no longer lists is
/// `Expired`), never `Unknown`-waited to the 45 s deadline. Arm (b): the
/// dead member's other 63 grants are swept with it, so a second pass on
/// one of them recalls nobody and waits for nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_evicted_readers_grants_are_swept_at_the_owners_eviction_not_at_the_deadline() {
    use squeezefs::membership::{
        self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    };
    let _g = SEAM.lock().await;
    membership::uninstall();
    let ticks = Arc::new(AtomicU64::new(10_000));
    let owner = MembershipOwner::arm(
        "tok-owner",
        3,
        2,
        LeaseClocks::derive(Duration::from_micros(250)).expect("the shipped derivation"),
        LeaseClock::manual(Arc::clone(&ticks)),
    )
    .expect("arm the owner");
    membership::install_owner(Arc::clone(&owner));
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    let mut files = Vec::new();
    for i in 0..64 {
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
    let client = "reader-dead";
    let JoinOutcome::Granted(_) = owner.join(JoinRequest {
        id: client.to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-sym-coherence".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) else {
        panic!("the reader joins as a member");
    };
    let (reader, plane) = open_token_reader(&path, &endpoint, client).await;
    for &f in &files {
        let _ = Metadata::getattr(reader.as_ref(), f).await.unwrap();
    }
    assert_eq!(holder.stats().outstanding, 64);
    // Killed: the channel is gone and no ack ever travels.
    plane.test_kill();
    wait_until("the reader's channel task exited", || {
        !plane.stats().channel_alive
    })
    .await;

    // (a) A pass recalling one of its tokens parks on the dead reader.
    let w = Arc::clone(&writer);
    let f0 = files[0];
    let commit = tokio::spawn(async move {
        Metadata::setattr(
            w.as_ref(),
            f0,
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
    wait_until("the recall was issued", || holder.stats().recalls == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!commit.is_finished(), "the pass waits on the dead reader");
    // The owner's cadence sweep: the lease is past, the member is EVICTED.
    ticks.fetch_add(
        owner.clocks().t_owner.as_millis() as u64 + 1,
        Ordering::SeqCst,
    );
    let evicted = owner.expire_due();
    assert!(
        evicted.iter().any(|e| e.id == client),
        "the sweep evicted the dead reader"
    );
    assert!(
        owner.lease_deadline_ms(client).is_none(),
        "the owner no longer lists it"
    );
    // The token-client registry is bounded by the census (review round 3,
    // Issue 28): the departed member's id left it with its grants.
    assert!(
        !squeezefs::meta_ship::token_plane::is_token_client(client),
        "a departed member is no longer a token client"
    );
    let t = std::time::Instant::now();
    commit.await.unwrap().unwrap();
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "the pass completed AT the eviction ({:?}), not at a tick or the deadline",
        t.elapsed()
    );
    let s = holder.stats();
    assert_eq!(s.recall_acks, 0);
    assert_eq!(
        s.expired_with_lease, 1,
        "the recalled grant expired with the lease"
    );
    assert_eq!(
        s.lease_swept_grants, 63,
        "the dead member's other grants are gone with it"
    );
    assert_eq!(s.timeouts_live, 0);
    assert_eq!(s.outstanding, 0);
    // (b) A second pass on one of them recalls nobody.
    let t = std::time::Instant::now();
    Metadata::setattr(
        writer.as_ref(),
        files[1],
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
    assert!(t.elapsed() < Duration::from_millis(500));
    assert_eq!(holder.stats().recalls, 1, "no new recall");
    membership::uninstall();
    host.shutdown();
    shutdown(&writer).await;
}

/// **The holder grants members only** (review round 3, Issue 27): where
/// a membership owner is installed, a token verb from a client it does
/// not list is REFUSED before anything is granted or registered — the
/// owner-side lease law enforced at the grant, not first at the recall
/// (a non-member's grant read `Expired` at its first recall and never
/// blocked a writer, but it served from a token no lease backed and was
/// counted a token client). A ghost with the cluster secret takes no
/// token (its read fails closed, `dlm_token_nonmember_refusals` +1, not a
/// token client, `grants_served` unmoved); a member joined through the
/// owner is granted as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_holder_grants_members_only_and_refuses_a_ghost_before_it_registers() {
    use squeezefs::membership::{
        self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    };
    let _g = SEAM.lock().await;
    membership::uninstall();
    squeezefs::meta_ship::token_plane::test_clear_token_clients();
    let ticks = Arc::new(AtomicU64::new(10_000));
    let owner = MembershipOwner::arm(
        "tok-owner-27",
        3,
        2,
        LeaseClocks::derive(Duration::from_micros(250)).expect("the shipped derivation"),
        LeaseClock::manual(Arc::clone(&ticks)),
    )
    .expect("arm the owner");
    membership::install_owner(Arc::clone(&owner));
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    let f = Metadata::create(writer.as_ref(), 1, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;

    // The ghost: the secret, no membership lease. Armed directly (its
    // recall channel never becomes fresh — every verb is refused).
    let ghost = open_routed_meta_set_read_only(&[path.display().to_string()])
        .await
        .expect("read-only open");
    let ghost_plane = ghost.volumes[0]
        .arm_token_reader(TokenClientConfig {
            endpoint: endpoint.to_string(),
            secret: SECRET.to_vec(),
            client_id: "ghost".to_string(),
            volume: 0,
        })
        .expect("the client arms; the holder decides");
    let served0 = holder.stats().grants_served;
    // The ghost's standing poll is the first verb the holder sees from it
    // — refused as a non-member (the channel never becomes fresh).
    wait_until("the holder refused the ghost's poll", || {
        holder.stats().nonmember_refusals >= 1
    })
    .await;
    let e = Metadata::getattr(ghost.as_ref(), f)
        .await
        .expect_err("a ghost's read fails closed");
    assert!(
        e.to_string().contains("read token unavailable"),
        "fails closed under R-SYM-4: {e}"
    );
    assert_eq!(holder.stats().grants_served, served0, "nothing granted");
    assert_eq!(holder.stats().outstanding, 0);
    assert!(
        !squeezefs::meta_ship::token_plane::is_token_client("ghost"),
        "a refused ghost is not a token client"
    );
    // The refusal's OWN text reaches the client (PR 12b round 3, F8): the
    // membership screen answers STATUS_REFUSED with a reason string, not
    // an encoded reply — decoded as one it read `invalid value: integer
    // 99` and a joiner re-enrolling at a failover successor surfaced a
    // user op's EIO off the garbage; it is the retryable class.
    let e = ghost_plane
        .probe()
        .await
        .expect_err("a ghost's probe is refused");
    assert!(
        e.to_string().contains("holds no live membership lease"),
        "the refusal's own text, never a decode of it: {e}"
    );
    assert!(!e.to_string().contains("undecodable"), "{e}");
    assert!(
        matches!(&e, squeezefs::error::SqueezefsError::Refused { errno, .. } if *errno == libc::EAGAIN),
        "the retryable class: {e:?}"
    );
    assert_eq!(ghost_plane.stats().grants, 0);
    ghost_plane.stop().await;
    shutdown(&ghost).await;

    // A member joined through the owner is granted as before.
    let member = "reader-member";
    let JoinOutcome::Granted(_) = owner.join(JoinRequest {
        id: member.to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-sym-coherence".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) else {
        panic!("the reader joins as a member");
    };
    let refusals = holder.stats().nonmember_refusals;
    let (reader, plane) = open_token_reader(&path, &endpoint, member).await;
    let _ = Metadata::getattr(reader.as_ref(), f).await.unwrap();
    assert_eq!(holder.stats().grants_served, served0 + 1);
    assert_eq!(
        holder.stats().nonmember_refusals,
        refusals,
        "a member is never refused"
    );
    assert!(squeezefs::meta_ship::token_plane::is_token_client(member));
    plane.stop().await;
    shutdown(&reader).await;
    membership::uninstall();
    host.shutdown();
    shutdown(&writer).await;
}

/// **A grant registered inside an eviction's window is judged `Expired`,
/// never `Unknown`** (review round 4, Issue 29 — Issue 28's departure
/// prune re-opened Issue 5's stall for a microsecond: a dispatch that
/// passed the membership check while the member was listed could land
/// its `grant_register` AFTER the eviction ran `sweep_departed` + the
/// prune, leaving a grant of a client the owner does not list AND
/// `is_token_client` false — `Unknown`, waited like live to the
/// deadline). With members-only granting, any holder the owner does not
/// list has departed, so the verdict under an installed owner is
/// `Expired` unconditionally. The window made deterministic
/// (`TEST_DISPATCH_HOLD_AFTER_CHECK_MS`): the member's grant dispatch
/// parks after its check, the owner's sweep evicts it inside the park,
/// the grant lands — and the next recall completes at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_landing_inside_an_evictions_window_is_expired_not_unknown() {
    use squeezefs::membership::{
        self, JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    };
    use squeezefs::meta_ship::token_plane::TEST_DISPATCH_HOLD_AFTER_CHECK_MS;
    let _g = SEAM.lock().await;
    membership::uninstall();
    squeezefs::meta_ship::token_plane::test_clear_token_clients();
    let ticks = Arc::new(AtomicU64::new(10_000));
    let owner = MembershipOwner::arm(
        "tok-owner-29",
        3,
        2,
        LeaseClocks::derive(Duration::from_micros(250)).expect("the shipped derivation"),
        LeaseClock::manual(Arc::clone(&ticks)),
    )
    .expect("arm the owner");
    membership::install_owner(Arc::clone(&owner));
    let dir = tempfile::tempdir().unwrap();
    let path = format_stamped(dir.path(), "meta0").await;
    let writer = open_armed_writer(&path).await;
    let (host, endpoint) = holder_listener(&writer.volumes[0]);
    let holder = writer.volumes[0].token_holder().unwrap().clone();
    let f = Metadata::create(writer.as_ref(), 1, "f", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap()
        .ino;
    let client = "reader-window";
    let JoinOutcome::Granted(_) = owner.join(JoinRequest {
        id: client.to_string(),
        role: MemberRole::Reader,
        endpoint: None,
        pid: std::process::id(),
        boot: "boot-sym-coherence".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    }) else {
        panic!("the reader joins as a member");
    };
    let (reader, plane) = open_token_reader(&path, &endpoint, client).await;
    // The grant dispatch parks after its membership check.
    TEST_DISPATCH_HOLD_AFTER_CHECK_MS.store(400, Ordering::SeqCst);
    let r = Arc::clone(&reader);
    let fetch = tokio::spawn(async move { Metadata::getattr(r.as_ref(), f).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Inside the park: the lease passes and the owner's sweep EVICTS the
    // member — its (empty) grant set swept, its id pruned from the
    // token-client registry.
    ticks.fetch_add(
        owner.clocks().t_owner.as_millis() as u64 + 1,
        Ordering::SeqCst,
    );
    let evicted = owner.expire_due();
    assert!(
        evicted.iter().any(|e| e.id == client),
        "the sweep evicted it"
    );
    assert!(owner.lease_deadline_ms(client).is_none());
    // The dead member: its channel is gone, so no ack will ever travel
    // and the recall below can complete only by the lease's verdict.
    plane.test_kill();
    wait_until("the reader's channel task exited", || {
        !plane.stats().channel_alive
    })
    .await;
    // The parked dispatch resumes and the grant LANDS for a departed
    // client (the fetch itself fails closed at the reader's own gate —
    // the holder-side registration is the point).
    let _ = fetch.await.unwrap();
    TEST_DISPATCH_HOLD_AFTER_CHECK_MS.store(0, Ordering::SeqCst);
    wait_until("the window's grant is registered", || {
        holder.stats().outstanding >= 1
    })
    .await;
    // The recall of that grant completes AT ONCE — the departed holder's
    // lease reads Expired, never Unknown-waited to the deadline.
    let t = std::time::Instant::now();
    Metadata::setattr(
        writer.as_ref(),
        f,
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
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "the recall completed at once, not at the deadline ({:?})",
        t.elapsed()
    );
    let s = holder.stats();
    assert_eq!(s.timeouts_live, 0, "never the stuck-reader class");
    assert_eq!(s.outstanding, 0);
    assert!(s.expired_with_lease >= 1 || s.lease_swept_grants >= 1);
    plane.stop().await;
    shutdown(&reader).await;
    membership::uninstall();
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
    let deferred0 = free_grace::recall_timeout_deferrals();
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
    assert_eq!(free_grace::recall_timeout_deferrals(), deferred0);
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
    // PR 12: the reader dials no DECLARED authority (the knob is retired
    // under the plane) — the manager's endpoint is the binding, or the
    // listener its join ladder published into its claim-set entry. The
    // contracts bind it directly; an unpublished, unbound manager refuses.
    {
        let probe_reader = open_routed_meta_set_read_only(&[path.display().to_string()])
            .await
            .expect("read-only open");
        let (router, _b) = data_router("ro_mount_tokens_unbound").await;
        let err = ro_coherence::arm_token_readers(&probe_reader.volumes, &router)
            .await
            .expect_err("a manager that published no listener refuses the arm");
        assert!(
            err.contains("published no listener"),
            "the refusal names the missing binding: {err}"
        );
        assert!(
            !err.contains("SQUEEZEFS_MW_AUTHORITY"),
            "and never the retired knob: {err}"
        );
        shutdown(&probe_reader).await;
    }
    {
        let probe_reader = open_routed_meta_set_read_only(&[path.display().to_string()])
            .await
            .expect("read-only open");
        probe_reader.volumes[0].bind_reader_holder_endpoint(0, &dark.endpoint().to_string());
        let (router, _b) = data_router("ro_mount_tokens_dark").await;
        let err = ro_coherence::arm_token_readers(&probe_reader.volumes, &router)
            .await
            .expect_err("a holder that serves no tokens refuses the arm");
        assert!(
            err.contains("does not serve read tokens"),
            "the refusal names the probe's finding: {err}"
        );
        assert!(
            err.contains("join ladder"),
            "and the writer's posture: {err}"
        );
        shutdown(&probe_reader).await;
    }
    dark.shutdown();

    // Settle the WRITER's image first: a checkpoint's tail releases the
    // pending frees it covers AFTER its bitmap pages landed, so the next
    // cadence cycle writes them — the writer is quiet only once two
    // samples a cadence apart agree.
    let before = settled_device_digest(&path).await;
    let reader = open_routed_meta_set_read_only(&[path.display().to_string()])
        .await
        .expect("read-only open");
    reader.volumes[0].bind_reader_holder_endpoint(0, &endpoint);
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
    std::env::remove_var(SYMMETRIC_META_ENV);
    squeezefs::fuse_client::set_read_only_mount(false);
    shutdown(&writer).await;
}
