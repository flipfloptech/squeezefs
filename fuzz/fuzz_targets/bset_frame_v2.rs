//! Fuzz the **bset frame v2** grammar and the §5.8.2 frame screen
//! (`src/meta_backend/kv/node.rs` — design-symmetric-metadata PR 5): the
//! stamped 40 B frame `magic ‖ version=2 ‖ reserved ‖ node_seq ‖ padded_len
//! ‖ bset_len ‖ appender_id ‖ g ‖ checksum`, dispatched by the LAYOUT
//! (a v1 frame on a v2 layout and a v2 frame on a v1 layout are both
//! foreign), walked under the four-rule screen (rule 4: a current-generation
//! frame from an appender that is not the lessee).
//!
//! Three arms: (1) arbitrary bytes through the screened walk on a v2
//! layout under an arbitrary screen — total, bsets in bounds, the tail
//! never past the extent; (2) the same bytes on a v1 layout — the shipped
//! grammar, byte for byte; (3) a CONSTRUCTIVE log — frames encoded under
//! fuzzer-chosen stamps, walked under a fuzzer-chosen screen — whose
//! verdict must equal the pure rule function applied frame by frame
//! (the screen is one function, applied by every loader).
#![no_main]

use arbitrary::Arbitrary;
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::node::{
    encode_bset_frame, verify_node_extent_screened, FrameScreen, FrameStamp, NodeLayout,
    NodeWriteParams, NODE_PAGE,
};
use squeezefs::meta_backend::kv::record::{inode_key, InodeValue, Record, TREE_INODES};

const NODE_SIZE: usize = 64 * 1024;

#[derive(Arbitrary, Debug)]
struct Input {
    raw: Vec<u8>,
    g_current: u32,
    appender_current: u32,
    tail_g: u32,
    tail_frame: u8,
    pr_fenced: bool,
    has_tail: bool,
    /// The constructive log: up to 8 frames of `(appender_id, g)`.
    frames: Vec<(u8, u8)>,
}

fn record(i: u64) -> Record {
    let v = InodeValue {
        mode: 0o100644,
        uid: i as u32,
        gid: 0,
        nlink: 1,
        flags: 0,
        rdev: 0,
        size: i,
        atime: i,
        mtime: i,
        ctime: i,
    };
    Record::put(inode_key(i + 1).to_vec(), i + 1, v.encode())
}

fuzz_target!(|input: Input| {
    let v2 = NodeLayout::new_symmetric(NODE_SIZE).expect("64 KiB is legal");
    let v1 = NodeLayout::new(NODE_SIZE).expect("64 KiB is legal");
    let screen = FrameScreen {
        g_current: input.g_current,
        appender_current: input.appender_current,
        recorded_tail: input.has_tail.then_some((
            input.tail_g,
            (u32::from(input.tail_frame) + 1) * NODE_PAGE as u32,
        )),
        pr_fenced: input.pr_fenced,
    };

    // Arms 1 + 2: arbitrary bytes, both layouts, screened and not.
    let mut buf = vec![0u8; NODE_SIZE];
    let n = input.raw.len().min(NODE_SIZE);
    buf[..n].copy_from_slice(&input.raw[..n]);
    let claimed = u64::from_le_bytes(buf[8..16].try_into().unwrap()) & !0xFFF;
    for addr in [0u64, claimed] {
        for (layout, sc) in [(&v2, Some(&screen)), (&v2, None), (&v1, None)] {
            let Ok(node) =
                verify_node_extent_screened(Bytes::from(buf.clone()), layout, addr, 0, sc)
            else {
                continue;
            };
            assert!(node.tail_offset() <= NODE_SIZE);
            for i in 0..node.bset_count() {
                node.bset(i).expect("an accepted bset must re-parse");
            }
        }
    }

    // Arm 3: a constructive log under the screen equals the rule function.
    let frames: Vec<FrameStamp> = input
        .frames
        .iter()
        .take(8)
        .map(|&(a, g)| FrameStamp {
            appender_id: u32::from(a),
            g: u32::from(g),
        })
        .collect();
    if frames.is_empty() {
        return;
    }
    let node_seq = 0x5EED_u64;
    let mut image = vec![0u8; NODE_SIZE];
    let header = squeezefs::meta_backend::kv::node::encode_header_page(
        &v2,
        &NodeWriteParams {
            node_addr: 0,
            node_seq,
            tree_id: TREE_INODES,
            level: 0,
            min_key: b"",
            max_key: &[0xFF; 16],
        },
    )
    .expect("header");
    image[..NODE_PAGE].copy_from_slice(&header);
    let mut pos = NODE_PAGE;
    let mut expected_kept = 0usize;
    let mut expected_stop: Option<u8> = None;
    let mut prev_g = None;
    for (i, s) in frames.iter().enumerate() {
        let frame =
            encode_bset_frame(&v2.stamped(*s), node_seq, &[record(i as u64)], 1).expect("frame");
        if pos + frame.len() > NODE_SIZE {
            break;
        }
        image[pos..pos + frame.len()].copy_from_slice(&frame);
        if expected_stop.is_none() {
            match screen.foreign_rule(*s, pos, prev_g) {
                Some(rule) => expected_stop = Some(rule),
                None => {
                    expected_kept += 1;
                    prev_g = Some(s.g);
                }
            }
        }
        pos += frame.len();
    }
    match verify_node_extent_screened(Bytes::from(image), &v2, 0, 0, Some(&screen)) {
        Ok(node) => {
            assert_eq!(
                node.bset_count(),
                expected_kept,
                "the walk kept exactly the rule's set"
            );
            assert_eq!(
                node.foreign_frames_screened(),
                u64::from(expected_stop.is_some()),
                "one screened stop iff the rule stopped the walk"
            );
        }
        Err(e) => {
            // The ONE refusal a well-formed log can earn: a screened frame
            // with a current-generation frame behind it (the overwrite
            // class).
            assert!(
                expected_stop.is_some()
                    && e.to_string().contains("foreign_frame_overwrite_detected"),
                "{e}"
            );
        }
    }
});
