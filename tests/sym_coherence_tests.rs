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
