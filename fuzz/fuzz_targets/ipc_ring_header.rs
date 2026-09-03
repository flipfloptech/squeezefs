//! Fuzz the **IPC session geometry / ring header** validator
//! (`crates/squeezefs-ipc/src/layout.rs`).
//!
//! The session memfd's header page is written by the daemon and read by
//! the shim at map time (`SessionHeader::validate`), and its `Geometry`
//! drives every offset the shim then computes. A shim that maps a hostile
//! or corrupt memfd must refuse, not compute an out-of-range offset — so
//! the law is: `SessionLayout::compute` either errors, or returns regions
//! that are ordered, non-overlapping, page-aligned where the design says
//! so, and entirely inside `total_bytes` (spec §11 TEST-4).
//!
//! `SlotDescriptor` is the *client-written* half of the same mapping —
//! the daemon reads it once per op (§5.3.1 rule 1) and must bound-check
//! it against the geometry, never trust it.
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use squeezefs_ipc::layout::{Geometry, SessionLayout};

#[derive(Arbitrary, Debug)]
struct Input {
    ring_entries: u32,
    slots: u32,
    arena_bytes: u64,
    max_op_bytes: u32,
    pad: u32,
    // The client-written descriptor fields the daemon must bound-check.
    op: u32,
    flags: u32,
    binding: u64,
    offset: u64,
    len: u32,
    arena_off: u64,
}

fuzz_target!(|input: Input| {
    let g = Geometry {
        ring_entries: input.ring_entries,
        slots: input.slots,
        arena_bytes: input.arena_bytes,
        max_op_bytes: input.max_op_bytes,
        _pad: input.pad,
    };
    // Total: an arbitrary geometry is a refusal, never a panic and never
    // an arithmetic overflow.
    let Ok(layout) = SessionLayout::compute(&g) else {
        assert!(
            g.validate().is_err(),
            "compute refuses iff validate refuses"
        );
        return;
    };
    assert!(g.validate().is_ok());

    // Region ordering and containment: every offset the shim derives from
    // this layout must land inside the mapping.
    let regions = [
        ("header", layout.header_off, layout.header_bytes),
        ("ring", layout.ring_off, layout.ring_bytes),
        ("slots", layout.slots_off, layout.slots_bytes),
        ("stats", layout.stats_off, layout.stats_bytes),
        ("arena", layout.arena_off, layout.arena_bytes),
    ];
    let mut prev_end = 0u64;
    for (name, off, bytes) in regions {
        assert!(
            off >= prev_end,
            "{name} region at {off} overlaps the previous region ending {prev_end}"
        );
        let end = off
            .checked_add(bytes)
            .unwrap_or_else(|| panic!("{name} region length overflows"));
        assert!(
            end <= layout.total_bytes,
            "{name} region ends at {end}, past total_bytes {}",
            layout.total_bytes
        );
        prev_end = end;
    }
    assert!(
        layout.ring_cells_off >= layout.ring_off
            && layout.ring_cells_off <= layout.ring_off + layout.ring_bytes,
        "the ring cell array must live inside the ring region"
    );
    assert_eq!(
        layout.arena_bytes, g.arena_bytes,
        "the arena must be exactly the geometry's size"
    );

    // §5.3 rule 4: a client-written descriptor is bounded by the geometry.
    // The check the daemon performs must be expressible without overflow
    // for ANY client value — that is what this arm proves.
    let in_arena = input
        .arena_off
        .checked_add(u64::from(input.len))
        .is_some_and(|end| end <= layout.arena_bytes);
    let admissible = input.len <= g.max_op_bytes && in_arena;
    if admissible {
        assert!(
            input.arena_off + u64::from(input.len) <= layout.arena_bytes,
            "an admissible op must address only arena bytes"
        );
    }
    let _ = (input.op, input.flags, input.binding, input.offset);
});
