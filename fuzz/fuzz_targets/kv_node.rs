//! Fuzz the whole on-disk **node extent** grammar
//! (`src/meta_backend/kv/node.rs::verify_node_extent`) — header page,
//! self-address and geometry checks, the §4.5 append walk, and the
//! torn-tail diagnosis pass.
//!
//! This is the single most exposed on-disk decoder: every mount walks
//! every node it touches, and the §4.5 contract explicitly *expects*
//! garbage in the tail (a power-loss torn append). The law under test is
//! that arbitrary bytes produce either a `LoadedNode` whose bsets are all
//! in-bounds and re-parse, or a loud `KvError` — never a panic, never an
//! unbounded allocation, and never a branch on unverified bytes.
#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::node::{verify_node_extent, NodeLayout};

// The smallest legal node geometry (64 KiB, the `--meta-node-kib` floor):
// a fuzz corpus of 256 KiB entries would waste most of the budget on
// memcpy, and every branch in the walk is reachable at the floor.
const NODE_SIZE: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    let layout = NodeLayout::new(NODE_SIZE).expect("64 KiB is a legal node size");
    // Right-size the input to one extent: short inputs are zero-extended
    // (a fresh/unwritten extent), long ones truncated.
    let mut buf = vec![0u8; NODE_SIZE];
    let n = data.len().min(NODE_SIZE);
    buf[..n].copy_from_slice(&data[..n]);

    // Two arms: an address the header will not match (misdirected-read
    // detection) and the one the fuzzer's own header bytes claim, which is
    // how the walk past the header is ever reached.
    for node_addr in [0u64, header_claimed_addr(&buf)] {
        // Only 4 KiB-aligned addresses are legal; the checker rejects the
        // rest before touching the buffer.
        let node_addr = node_addr & !0xFFF;
        for durable_tail in [0u64, u64::MAX / 2] {
            let Ok(node) =
                verify_node_extent(Bytes::from(buf.clone()), &layout, node_addr, durable_tail)
            else {
                continue;
            };
            assert_eq!(node.header().node_addr, node_addr);
            assert_eq!(node.header().node_size as usize, NODE_SIZE);
            assert!(
                node.tail_offset() <= NODE_SIZE,
                "the append cursor must stay inside the extent"
            );
            for i in 0..node.bset_count() {
                // Every accepted range was verified at load, so this
                // cannot fail — if it does, the load-time promise is a lie.
                node.bset(i).expect("an accepted bset must re-parse");
            }
            let views = node
                .bset_views_newest_first()
                .expect("accepted bsets re-parse newest-first too");
            assert_eq!(views.len(), node.bset_count());
        }
    }
});

/// The `node_addr` the candidate header claims (offset 8, LE u64 — the
/// same field `NodeHeader::decode_page` reads). Feeding it back is what
/// lets a mutated header survive the self-address check and reach the
/// interesting code.
fn header_claimed_addr(buf: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[8..16]);
    u64::from_le_bytes(b)
}
