//! Derivation-sweep contracts (2026-08-04; user directive 2026-08-02:
//! "remove any hard coded limitations in favor of dynamically computed
//! values based on available system resources").
//!
//! Every class-A conversion from the sweep gets its tie test here (the
//! `il_sessions_default` drift-is-red pattern): the pure `resolve_*` /
//! `derived_*` form is pinned on BOTH canonical box shapes —
//!
//! - the **field shape**: 251 GB RAM client ⇒ resolved budget ≈ 176 GiB
//!   (70 % default resolution), 32 CPUs, 4 MiB block volumes, 32 MiB
//!   journal rings — the reviewer's sanity column;
//! - the **floor shape**: 4 GB / 2-CPU box ⇒ budget ≈ 2.8 GiB — floors
//!   must be physical minima or the never-regress-below-shipped posture
//!   (the `Q_DEPTH_FLOOR` house law), never tuning.
//!
//! Precedence law everywhere (the 2026-08-02 ipc-cap precedent):
//! **absolute env > percentage env > derived default**, explicit wins
//! verbatim, a bad env string never fails a mount (warn + fall through).
//!
//! Evidence note: `.benchmarks/2026-08-04-derivation-sweep.md`.

use squeezefs::fuse_client;
use squeezefs::mem_budget;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// The field client's resolved budget (251 GB RAM × 70 % ≈ 176 GiB).
const FIELD_BUDGET: u64 = 176 * GIB;
/// The floor box's resolved budget (4 GiB RAM × 70 % = 2.8 GiB).
const FLOOR_BUDGET: u64 = 2 * GIB + 820 * MIB;

// ---------------------------------------------------------------------------
// A1 — transport payload-buffer cap: budget fraction, no fixed ceiling
// ---------------------------------------------------------------------------

/// The former fixed 2 GiB `TRANSPORT_BUFFER_CAP_CEILING` is DELETED (the
/// ipc-arena-cap sibling, now converted): the cap is the budget/8
/// fraction alone. The pinned-arena bound is STRUCTURAL, not a byte
/// constant — the geometry never registers more than
/// `nqueues × Q_DEPTH_DESIRED × payload_sz` (the demand cap), so the
/// ceiling's only field effect was degrading depth on > 64-CPU big-RAM
/// boxes.
#[test]
fn transport_buffer_cap_is_budget_fraction_no_fixed_ceiling() {
    // Small budgets: byte-identical to the old law (ceiling never bound).
    assert_eq!(
        mem_budget::transport_buffer_cap(6 * GIB + 410 * MIB),
        (6 * GIB + 410 * MIB) / 8
    );
    // The decomposition box (76 GiB budget): the old ceiling clamped this
    // to 2 GiB and pinned per-queue depth below the measured-best 32 on
    // > 64-CPU geometries. Derived: 9.5 GiB.
    assert_eq!(mem_budget::transport_buffer_cap(76 * GIB), 76 * GIB / 8);
    // The field shape: 22 GiB — covers the 32-queue × depth-32 × 1 MiB
    // demand (1 GiB) with the depth ladder never engaging.
    assert_eq!(mem_budget::transport_buffer_cap(FIELD_BUDGET), 22 * GIB);
    // Tiny/unknown budgets degrade to 0 — the geometry floor (depth 4)
    // then holds the pre-L1 footprint (never-regress).
    assert_eq!(mem_budget::transport_buffer_cap(0), 0);
}

/// §5.7-style resolution: `SQUEEZEFS_TRANSPORT_MEM_MAX` (MiB, absolute,
/// explicit-wins-verbatim — the A0 lever: `2048` restores the retired
/// ceiling exactly) > `SQUEEZEFS_TRANSPORT_MEM_PCT` (percent of budget,
/// clamp (0,100]) > derived budget/8.
#[test]
fn transport_cap_resolution_precedence_absolute_pct_default() {
    use mem_budget::resolve_transport_buffer_cap;
    // Absolute wins over pct and default (incl. the old-ceiling lever).
    assert_eq!(
        resolve_transport_buffer_cap(8 * GIB, Some("2048"), Some("50")),
        2 * GIB
    );
    assert_eq!(resolve_transport_buffer_cap(8 * GIB, Some("0"), None), 0);
    // Pct next.
    assert_eq!(
        resolve_transport_buffer_cap(8 * GIB, None, Some("50")),
        4 * GIB
    );
    assert_eq!(
        resolve_transport_buffer_cap(8 * GIB, None, Some("12.5")),
        GIB
    );
    // Derived default.
    assert_eq!(resolve_transport_buffer_cap(8 * GIB, None, None), GIB);
    // Knob-family hygiene: garbage warns and falls through, never fails.
    assert_eq!(
        resolve_transport_buffer_cap(8 * GIB, Some("lots"), Some("25")),
        2 * GIB
    );
    assert_eq!(
        resolve_transport_buffer_cap(8 * GIB, Some("junk"), Some("nope")),
        GIB
    );
    // Pct clamps into (0, 100].
    assert_eq!(
        resolve_transport_buffer_cap(8 * GIB, None, Some("250")),
        8 * GIB
    );
    assert_eq!(resolve_transport_buffer_cap(8 * GIB, None, Some("-3")), GIB);
}

// ---------------------------------------------------------------------------
// A4 — meta node-cache budget: fraction of the R5 budget, shipped floor
// ---------------------------------------------------------------------------

/// The flat 512 MiB default becomes `max(budget/16, 512 MiB)`: the
/// fraction is scale-free (the budget itself is machine-derived), and the
/// floor is the never-regress-below-shipped posture (every box ran
/// 512 MiB before this sweep). Precedence: `SQUEEZEFS_META_NODE_CACHE_MB`
/// absolute-verbatim > `SQUEEZEFS_META_NODE_CACHE_PCT` > derived.
#[test]
fn node_cache_budget_derives_from_memory_budget() {
    use squeezefs::meta_backend::kv::backend::resolve_node_cache_budget;
    // Field shape: 176 GiB / 16 = 11 GiB.
    assert_eq!(
        resolve_node_cache_budget(FIELD_BUDGET, None, None),
        11 * GIB
    );
    // Floor shape: 2.8 GiB / 16 = 179 MiB < the shipped 512 MiB ⇒ floor
    // holds (byte-identical to the pre-sweep posture on small boxes).
    assert_eq!(
        resolve_node_cache_budget(FLOOR_BUDGET, None, None),
        512 * MIB
    );
    // Absolute env wins verbatim (the A0 lever: 512 restores old law).
    assert_eq!(
        resolve_node_cache_budget(FIELD_BUDGET, Some("512"), Some("50")),
        512 * MIB
    );
    // Pct spelling.
    assert_eq!(
        resolve_node_cache_budget(16 * GIB, None, Some("25")),
        4 * GIB
    );
    // Garbage falls through per the knob-family convention.
    assert_eq!(
        resolve_node_cache_budget(FLOOR_BUDGET, Some("many"), Some("nan")),
        512 * MIB
    );
}

// ---------------------------------------------------------------------------
// Dentry-class entry-count capacity: max(50_000, RAM / 200_000) — ONE fn
// ---------------------------------------------------------------------------

/// `mem_budget::dir_entry_capacity` is the one derivation the FUSE dentry
/// cache, its `..` parent memo and the cross-owner directory-parent memo
/// size by (PR 6 review round 2, Issue 27): one entry per 200 KB of the
/// shared RAM, floored at the shipped 50 000. A second literal is the
/// drift this tie makes red.
#[test]
fn dir_entry_capacity_is_one_derivation_with_the_shipped_floor() {
    use squeezefs::mem_budget::dir_entry_capacity;
    // Field shape: 176 GiB / 200 KB ≈ 944 k entries.
    assert_eq!(dir_entry_capacity(176 * GIB), 176 * GIB / 200_000);
    // Floor shape: 2.8 GiB / 200 KB ≈ 15 k < 50 k ⇒ the shipped floor.
    assert_eq!(dir_entry_capacity(FLOOR_BUDGET), 50_000);
    // The boundary: exactly at the floor's RAM the two agree.
    assert_eq!(dir_entry_capacity(50_000 * 200_000), 50_000);
    assert_eq!(dir_entry_capacity(50_001 * 200_000), 50_001);
    // The literal the two sites once carried, as the tie.
    for total in [0u64, 1 << 30, 64 << 30, 1 << 40] {
        assert_eq!(
            dir_entry_capacity(total),
            std::cmp::max(50_000, total / 200_000)
        );
    }
}

// ---------------------------------------------------------------------------
// A5 — checkpoint dirty-node cap: budget/32 ÷ node_size, shipped floor
// ---------------------------------------------------------------------------

/// The flat 4096 becomes `max(budget/32 / node_size, 4096)`: the cap
/// bounds RAM pinned by dirty nodes AND the mount-replay working set, so
/// it scales with the budget; the floor is the shipped posture (a lower
/// cap checkpoints more often than any box ever shipped — pure overhead).
/// Env stays absolute-verbatim.
#[test]
fn checkpoint_dirty_cap_derives_from_budget_and_node_size() {
    use squeezefs::meta_backend::kv::checkpoint::resolve_max_dirty_nodes;
    let node = 256 * 1024u64; // the default 256 KiB node
                              // Field: 176 GiB / 32 / 256 KiB = 22528 nodes (5.5 GiB dirty ceiling).
    assert_eq!(resolve_max_dirty_nodes(FIELD_BUDGET, node, None), 22528);
    // Floor shape: 2.8 GiB / 32 = 89.6 MiB ⇒ 358 nodes < 4096 ⇒ floor.
    assert_eq!(resolve_max_dirty_nodes(FLOOR_BUDGET, node, None), 4096);
    // Bigger nodes lower the count for the same byte ceiling.
    assert_eq!(
        resolve_max_dirty_nodes(FIELD_BUDGET, 1024 * 1024, None),
        5632
    );
    // Env verbatim (the A0 lever).
    assert_eq!(
        resolve_max_dirty_nodes(FIELD_BUDGET, node, Some("4096")),
        4096
    );
    assert_eq!(resolve_max_dirty_nodes(FIELD_BUDGET, node, Some("64")), 64);
    // Garbage falls through.
    assert_eq!(
        resolve_max_dirty_nodes(FLOOR_BUDGET, node, Some("heaps")),
        4096
    );
    // node_size 0 must not divide-by-zero (defensive: treat as default).
    assert_eq!(resolve_max_dirty_nodes(FLOOR_BUDGET, 0, None), 4096);
}

// ---------------------------------------------------------------------------
// A6 — commit-conveyor batch caps: cores / journal-ring derived
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_META_COMMIT_BATCH_TXS` default: `max(64, cpus × 2)` —
/// committer arrivals scale with handler parallelism (queues = possible
/// CPUs); floor 64 = the shipped M7 posture. Field 32-CPU box: 64
/// (byte-identical).
#[test]
fn commit_batch_txs_derives_from_cores() {
    use squeezefs::meta_backend::kv::backend::resolve_commit_batch_txs;
    assert_eq!(resolve_commit_batch_txs(None, 32), 64, "field shape");
    assert_eq!(resolve_commit_batch_txs(None, 2), 64, "floor shape");
    assert_eq!(resolve_commit_batch_txs(None, 128), 256);
    assert_eq!(resolve_commit_batch_txs(Some("8"), 128), 8, "env verbatim");
    assert_eq!(resolve_commit_batch_txs(Some("0"), 32), 64, "0 invalid");
    assert_eq!(resolve_commit_batch_txs(Some("junk"), 32), 64);
}

/// `SQUEEZEFS_META_COMMIT_BATCH_BYTES` default: `max(256 KiB, ring
/// user-capacity / 16)` — the batch is a fixed fraction of the ring so
/// ≥ 16 batch reservations always cycle (the liveness-margin shape); the
/// senior per-volume clamp to the ring's admissible capacity is
/// unchanged. Floor 256 KiB = the shipped posture.
#[test]
fn commit_batch_bytes_derives_from_journal_ring() {
    use squeezefs::meta_backend::kv::backend::resolve_commit_batch_bytes;
    // 32 MiB-ring volumes (the field default at ≥ 2 GiB meta volumes):
    // ~2 MiB batches.
    assert_eq!(resolve_commit_batch_bytes(None, 32 * MIB), 2 * MIB);
    // 8 MiB-ring volumes: 512 KiB.
    assert_eq!(resolve_commit_batch_bytes(None, 8 * MIB), 512 * KIB);
    // Tiny rings floor at the shipped 256 KiB (the senior admissible
    // clamp at the call site still bounds it to the ring).
    assert_eq!(resolve_commit_batch_bytes(None, MIB), 256 * KIB);
    // Env verbatim.
    assert_eq!(resolve_commit_batch_bytes(Some("65536"), 32 * MIB), 65536);
    assert_eq!(resolve_commit_batch_bytes(Some("0"), 32 * MIB), 2 * MIB);
}

const KIB: u64 = 1024;

// ---------------------------------------------------------------------------
// D-3 — DLM-class stripe widths: the transport's delivered concurrency
// ---------------------------------------------------------------------------

/// The width law (`stripe_locks::derived_stripe_width`), pinned over the
/// canonical grid: `next_pow2(max(shipped, 16 × possible_cpus ×
/// q_depth))`. The product is the concurrency the FUSE-over-io_uring
/// transport can present (one queue per kernel possible CPU × the
/// per-queue depth); ×16 is the load-factor target (α ≤ 1/16 — a
/// false-sharing wait, which on the 4a tables costs a whole commit, on
/// at most one acquire in sixteen at full ring occupancy); `shipped` is
/// the never-regress floor. Every width is a power of two ≥ shipped and
/// ≥ 16 × cpus × depth, and the law is monotone in both inputs.
#[test]
fn dlm_stripe_width_derives_from_possible_cpus_times_q_depth() {
    use squeezefs::stripe_locks::{
        derived_stripe_width, DLM_STRIPES_SHIPPED, LEASE_STRIPES_SHIPPED, STRIPE_LOAD_FACTOR_INV,
    };
    assert_eq!(
        STRIPE_LOAD_FACTOR_INV, 16,
        "the α ≤ 1/16 load-factor target"
    );
    assert_eq!(
        DLM_STRIPES_SHIPPED, 4096,
        "the shipped 4a width is the floor"
    );
    assert_eq!(
        LEASE_STRIPES_SHIPPED, 1024,
        "the shipped waiter/serve width"
    );
    let cpus = [1usize, 2, 4, 8, 16, 32, 64, 128, 192];
    let depths = [4usize, 8, 16, 32];
    for shipped in [DLM_STRIPES_SHIPPED, LEASE_STRIPES_SHIPPED] {
        let mut prev_by_depth = [0usize; 4];
        for &c in &cpus {
            for (di, &d) in depths.iter().enumerate() {
                let w = derived_stripe_width(shipped, c, d);
                assert!(
                    w.is_power_of_two(),
                    "shipped {shipped} cpus {c} depth {d}: {w}"
                );
                assert!(
                    w >= shipped,
                    "never below the shipped floor ({shipped}): {w}"
                );
                assert!(
                    w >= 16 * c * d,
                    "≥ 16 × cpus × depth (16 × {c} × {d} = {}): {w}",
                    16 * c * d
                );
                assert!(
                    w < 2 * (16 * c * d).max(shipped),
                    "the next power of two, not beyond it: {w}"
                );
                assert!(w >= prev_by_depth[di], "monotone in cpus at depth {d}");
                prev_by_depth[di] = w;
                if di > 0 {
                    assert!(
                        w >= derived_stripe_width(shipped, c, depths[di - 1]),
                        "monotone in depth at {c} cpus"
                    );
                }
            }
        }
    }
    // The two canonical shapes, both classes.
    assert_eq!(
        derived_stripe_width(4096, 32, 32),
        16_384,
        "field 4a: 4× shipped"
    );
    assert_eq!(
        derived_stripe_width(1024, 32, 32),
        16_384,
        "field waiter/serve: 16×"
    );
    assert_eq!(
        derived_stripe_width(4096, 2, 4),
        4096,
        "floor box 4a: the shipped width"
    );
    assert_eq!(
        derived_stripe_width(1024, 2, 4),
        1024,
        "floor box waiter/serve: shipped"
    );
    // The depth-degraded field box (payload budget at the Q_DEPTH_FLOOR):
    // 32 × 4 × 16 = 2048 < 4096 — the 4a floor holds.
    assert_eq!(derived_stripe_width(4096, 32, 4), 4096);
    assert_eq!(derived_stripe_width(1024, 32, 4), 2048);
    // A 192-CPU box at depth 32: 98,304 → 131,072.
    assert_eq!(derived_stripe_width(4096, 192, 32), 131_072);
}

/// W-6: the transport's delivered-concurrency ceiling
/// (`stripe_locks::transport_inflight_ceiling`) is `possible_cpus ×
/// q_depth` — the same two terms the D-3 width law multiplies, WITHOUT the
/// load factor and the power-of-two rounding (it bounds a population, not
/// a mask). Its one consumer is the write-phase census map's retained
/// capacity (`2 ×` — the unit key + one block key per in-flight write),
/// which is what keeps a quiet mount from re-allocating that map's
/// bucket array on every write. Pinned against the process's own
/// `possible_cpus` and the sizing depth the D-3 law reads (the explicit
/// `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` clamped to the transport's
/// `1..=Q_DEPTH_DESIRED`, else `Q_DEPTH_DESIRED`) — drift in either term
/// is red here.
#[test]
fn transport_inflight_ceiling_is_possible_cpus_times_q_depth() {
    use squeezefs::stripe_locks::transport_inflight_ceiling;
    let cpus = squeezefs::cpu::possible_cpus().max(1);
    let depth = std::env::var("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|d| d.clamp(1, fuse3::raw::Q_DEPTH_DESIRED))
        .unwrap_or(fuse3::raw::Q_DEPTH_DESIRED);
    assert_eq!(
        transport_inflight_ceiling(),
        cpus * depth,
        "the ceiling is possible_cpus × q_depth, no factor, no rounding"
    );
    assert!(
        transport_inflight_ceiling() >= 1,
        "never zero — a zero minimum capacity is exactly the shrink-to-zero shape"
    );
}

/// `SQUEEZEFS_DLM_STRIPES` resolution: explicit wins verbatim (a non-power
/// of two rounds UP — the index is a mask), else the derived law. `4096`
/// is the shipped-4a control and `1` the everything-serializes crucible.
#[test]
fn dlm_stripes_knob_is_explicit_over_derived_and_rounds_up_to_pow2() {
    use squeezefs::stripe_locks::resolve_dlm_stripes;
    assert_eq!(resolve_dlm_stripes(None, 4096, 32, 32), 16_384, "derived");
    assert_eq!(
        resolve_dlm_stripes(Some(4096), 4096, 32, 32),
        4096,
        "the A/B control"
    );
    assert_eq!(
        resolve_dlm_stripes(Some(1024), 4096, 32, 32),
        1024,
        "explicit below shipped"
    );
    assert_eq!(
        resolve_dlm_stripes(Some(1), 4096, 32, 32),
        1,
        "the crucible"
    );
    assert_eq!(
        resolve_dlm_stripes(Some(1000), 4096, 32, 32),
        1024,
        "rounds up to pow2"
    );
    assert_eq!(resolve_dlm_stripes(Some(65_536), 1024, 2, 4), 65_536);
    // Registered, the ENG-10 way.
    let k = squeezefs::env_knobs::KNOBS
        .iter()
        .find(|k| k.key == squeezefs::stripe_locks::DLM_STRIPES_ENV)
        .expect("SQUEEZEFS_DLM_STRIPES is registered");
    assert_eq!(
        k.kind,
        squeezefs::env_knobs::Kind::Int { lo: 1, hi: 1 << 24 }
    );
    // The live tables were built at the law's width (the process reads
    // the knob once; this suite runs without it set).
    let live = squeezefs::stripe_locks::dlm_stripe_width();
    let q_depth = std::env::var("SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|d| d.clamp(1, fuse3::raw::Q_DEPTH_DESIRED))
        .unwrap_or(fuse3::raw::Q_DEPTH_DESIRED);
    let expect = resolve_dlm_stripes(
        std::env::var(squeezefs::stripe_locks::DLM_STRIPES_ENV)
            .ok()
            .and_then(|v| v.trim().parse().ok()),
        4096,
        squeezefs::cpu::possible_cpus(),
        q_depth,
    );
    assert_eq!(live, expect, "the live 4a width is the law's");
    assert_eq!(
        squeezefs::meta_backend::dlm::DlmLockManager::new().width(),
        live,
        "a fresh DlmLockManager is built at the live width"
    );
}

// ---------------------------------------------------------------------------
// A7/A8 — W1 patch cap + W2 fold byte trigger: block-size fractions
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_PATCH_MAX_BYTES` default: block_size/8 — the natural
/// denominator for a sub-block patch bound (512 KiB on the shipped 4 MiB
/// block: field-identical). Env keeps absolute-verbatim semantics incl.
/// the `0` A/B lever.
#[test]
fn patch_max_bytes_derives_from_block_size() {
    assert_eq!(
        fuse_client::derived_patch_max_bytes(4 * MIB),
        512 * KIB,
        "field/default block size: byte-identical to the shipped 512 KiB"
    );
    assert_eq!(fuse_client::derived_patch_max_bytes(MIB), 128 * KIB);
    assert_eq!(fuse_client::derived_patch_max_bytes(16 * MIB), 2 * MIB);
}

/// `SQUEEZEFS_FOLD_MAX_BYTES` default: block_size/4 (1 MiB on the shipped
/// 4 MiB block: field-identical). `SQUEEZEFS_FOLD_MAX_EXTENTS` stays the
/// measured amortization trigger (class C — filed, not converted).
#[test]
fn fold_max_bytes_derives_from_block_size() {
    assert_eq!(fuse_client::derived_fold_max_bytes(4 * MIB), MIB);
    assert_eq!(fuse_client::derived_fold_max_bytes(MIB), 256 * KIB);
}

// ---------------------------------------------------------------------------
// The inline-layout ceiling: default one page, bound = the KV value cap
// minus the layout framing
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_INLINE_MAX_BYTES`: the DEFAULT is one page
/// (`INLINE_MAX_FLOOR`, the derived default) — an inline payload rides the
/// metadata plane twice per write (journal entry + CoW node append, ≈ 2×
/// its bytes) against the staged path's fixed ≈ 0.3 KB per file, so one
/// page is where the record's payload costs about what a block mapping
/// does (`.benchmarks/2026-09-09-inline-raise-sweep-local.md`: 16 KiB
/// inline −34 % files/s and 59× the metadata bytes per file). The FORMAT
/// BOUND — the volume's xattr value cap `min(64 KiB, node_size/4)` minus
/// the layout wire's 4 KiB framing headroom, the largest payload one KV
/// value holds — is the MAXIMUM the override may name: 60 KiB at the
/// shipped 256 KiB node, 12 KiB at the 64 KiB node floor.
#[test]
fn inline_max_bytes_default_is_one_page_and_the_bound_is_the_kv_value_cap() {
    use squeezefs::meta_backend::kv::node::{
        xattr_value_cap, DEFAULT_NODE_SIZE, MAX_NODE_SIZE, MIN_NODE_SIZE,
    };
    use squeezefs::routing::{
        derived_inline_max_bytes, inline_max_bytes_ceiling, INLINE_MAX_CEILING, INLINE_MAX_FLOOR,
        LAYOUT_INLINE_HEADROOM,
    };
    assert_eq!(INLINE_MAX_FLOOR, 4096, "one page");
    // The default is the floor at EVERY node size: the derivation does not
    // scale with the geometry, because the cost it prices is per byte of
    // payload on the metadata plane, not per node.
    for node in [
        MIN_NODE_SIZE,
        128 * KIB as usize,
        DEFAULT_NODE_SIZE,
        MAX_NODE_SIZE,
    ] {
        let cap = xattr_value_cap(node);
        assert_eq!(
            derived_inline_max_bytes(cap),
            INLINE_MAX_FLOOR,
            "node {node}: the default ceiling is one page"
        );
        let bound = inline_max_bytes_ceiling(cap);
        assert_eq!(
            bound,
            cap - LAYOUT_INLINE_HEADROOM,
            "node {node}: the format bound"
        );
        assert!((INLINE_MAX_FLOOR..=INLINE_MAX_CEILING).contains(&bound));
        // The layout-delta wire carries the data key behind a u16 length.
        assert!(bound <= u16::MAX as usize);
    }
    assert_eq!(
        inline_max_bytes_ceiling(xattr_value_cap(DEFAULT_NODE_SIZE)),
        60 * KIB as usize,
        "shipped 256 KiB node: 64 KiB cap − 4 KiB framing"
    );
    assert_eq!(
        inline_max_bytes_ceiling(xattr_value_cap(MIN_NODE_SIZE)),
        12 * KIB as usize,
        "64 KiB node floor: 16 KiB cap − 4 KiB framing"
    );
    assert_eq!(
        inline_max_bytes_ceiling(xattr_value_cap(MAX_NODE_SIZE)),
        INLINE_MAX_CEILING,
        "the 1 MiB node hits the XATTR_SIZE_MAX cap: the override's top"
    );
    // The registered range is exactly [floor, format bound].
    let knob = squeezefs::env_knobs::lookup("SQUEEZEFS_INLINE_MAX_BYTES")
        .expect("SQUEEZEFS_INLINE_MAX_BYTES is registered");
    assert_eq!(
        knob.kind,
        squeezefs::env_knobs::Kind::Int {
            lo: INLINE_MAX_FLOOR as i128,
            hi: INLINE_MAX_CEILING as i128,
        }
    );
    assert_eq!(knob.default, "4096", "the documented default is one page");
}

// ---------------------------------------------------------------------------
// The small-file packer's own-block threshold: CHUNK/2 by the block
// economy; slots on the ranged-read LBA grain
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_PACK_MAX_SLOT_BYTES` (design-small-file-packing §5.5, KD-5):
/// the largest slot the packer shares a block for DERIVES from the block
/// economy — a tenant that takes its OWN block wastes `CHUNK − slot`
/// forever (a staged file never grows in place), a packed tenant costs
/// one compaction copy of `slot`, and the saving covers the copy iff
/// `slot ≤ CHUNK/2`. The same break-even is the compaction trigger. The
/// slot GRAIN is the conservative 4 KiB LBA every ranged device read
/// already assumes (design-read-path OQ #1, KD-6): `pack_slot_len` rounds
/// an image up to it, and the knob's floor is exactly one grain (the
/// one-LBA-tenants measurement posture). The knob is the measurement
/// lever — `CHUNK_SIZE` packs everything — never an operational posture.
#[test]
fn pack_max_slot_default_is_half_the_chunk_and_the_grain_is_the_lba_law() {
    use squeezefs::block_allocator::CHUNK_SIZE;
    use squeezefs::routing::{
        pack_max_slot_bytes, pack_slot_len, set_pack_max_slot_bytes_override,
    };
    let _serial = pack_knob_serial();

    // The default is the break-even, and it is CHUNK-derived (a 4 MiB
    // chunk ⇒ 2 MiB; the tie is the ratio, so a chunk change moves it).
    set_pack_max_slot_bytes_override(None);
    assert_eq!(
        pack_max_slot_bytes(),
        CHUNK_SIZE / 2,
        "own-block threshold = CHUNK/2"
    );
    assert_eq!(CHUNK_SIZE, 4 * MIB, "the shipped allocator chunk");

    // The slot grain: one LBA (4 KiB). An image is rounded UP to the
    // grain — 1 byte costs one grain, an exact multiple costs itself, one
    // byte past a multiple costs the next grain — and the pad is the
    // `pack_slot_pad_bytes` ledger's per-tenant term.
    assert_eq!(pack_slot_len(1), 4096);
    assert_eq!(pack_slot_len(4096), 4096);
    assert_eq!(pack_slot_len(4097), 8192);
    assert_eq!(pack_slot_len(64 * 1024 + 1), 68 * 1024);
    assert_eq!(pack_slot_len(CHUNK_SIZE), CHUNK_SIZE);

    // The explicit override wins verbatim (the measurement lever) and the
    // seam returns to the derivation.
    set_pack_max_slot_bytes_override(Some(4096));
    assert_eq!(pack_max_slot_bytes(), 4096, "one-LBA tenants only");
    set_pack_max_slot_bytes_override(Some(CHUNK_SIZE));
    assert_eq!(pack_max_slot_bytes(), CHUNK_SIZE, "pack everything");
    set_pack_max_slot_bytes_override(None);
    assert_eq!(pack_max_slot_bytes(), CHUNK_SIZE / 2);

    // The registered range is exactly [one grain, the chunk], and the
    // documented default names the derivation.
    let knob = squeezefs::env_knobs::lookup("SQUEEZEFS_PACK_MAX_SLOT_BYTES")
        .expect("SQUEEZEFS_PACK_MAX_SLOT_BYTES is registered");
    assert_eq!(
        knob.kind,
        squeezefs::env_knobs::Kind::Int {
            lo: 4096,
            hi: CHUNK_SIZE as i128,
        }
    );
    assert!(
        knob.default.contains("CHUNK_SIZE/2"),
        "the default line names the derivation, got {:?}",
        knob.default
    );

    // The lever itself: a registered Bool, ON since PK7's counted flip
    // (`.benchmarks/2026-09-10-packing-rows-squeeze-test.md`); `0` is the
    // A/B control — the one-block-per-file block arm, byte-identical to
    // the pre-flip shape.
    let lever = squeezefs::env_knobs::lookup("SQUEEZEFS_SMALL_FILE_PACKING")
        .expect("SQUEEZEFS_SMALL_FILE_PACKING is registered");
    assert_eq!(lever.kind, squeezefs::env_knobs::Kind::Bool);
    assert_eq!(lever.default, "on", "the lever ships ON since PK7");
}

/// PK6 (design-small-file-packing §5.5, §5.8, KD-5): the compaction
/// VICTIM threshold IS the own-block threshold — ONE law, two faces. A
/// block is worth compacting iff copying its live bytes reclaims at least
/// as many (`live ≤ CHUNK − live ⇔ live ≤ CHUNK/2`), which is exactly the
/// break-even that sizes the largest slot the packer shares a block for;
/// the measurement override moves BOTH faces together, so a knob value
/// can never make the mover chase blocks packing itself would not have
/// filled. Drift between the two functions is red here. (The seam is
/// process-global: this test and its neighbour above serialize on the
/// same lock.)
#[test]
fn pack_compaction_victim_threshold_is_the_own_block_threshold() {
    use squeezefs::block_allocator::CHUNK_SIZE;
    use squeezefs::defrag::{is_pack_victim, pack_victim_max_live_bytes};
    use squeezefs::routing::{pack_max_slot_bytes, set_pack_max_slot_bytes_override};
    let _serial = pack_knob_serial();

    set_pack_max_slot_bytes_override(None);
    assert_eq!(
        pack_victim_max_live_bytes(),
        pack_max_slot_bytes(),
        "the compaction trigger IS the own-block threshold (one derived law)"
    );
    assert_eq!(pack_victim_max_live_bytes(), CHUNK_SIZE / 2);
    // The break-even itself: at exactly half, copying `live` reclaims
    // `CHUNK − live = live` — worth it; one byte past half it is not.
    assert!(is_pack_victim(CHUNK_SIZE / 2));
    assert!(!is_pack_victim(CHUNK_SIZE / 2 + 1));
    assert!(is_pack_victim(4096), "a legacy one-LBA tenant block");
    assert!(
        is_pack_victim(0),
        "an empty block is trivially below the line"
    );

    // The measurement lever moves both faces in lockstep.
    set_pack_max_slot_bytes_override(Some(4096));
    assert_eq!(pack_victim_max_live_bytes(), pack_max_slot_bytes());
    assert_eq!(pack_victim_max_live_bytes(), 4096);
    assert!(is_pack_victim(4096));
    assert!(!is_pack_victim(8192));
    set_pack_max_slot_bytes_override(Some(CHUNK_SIZE));
    assert_eq!(pack_victim_max_live_bytes(), CHUNK_SIZE);
    assert!(is_pack_victim(CHUNK_SIZE));
    set_pack_max_slot_bytes_override(None);
    assert_eq!(pack_victim_max_live_bytes(), CHUNK_SIZE / 2);
}

/// The two pack-threshold ties share one process-global override seam.
fn pack_knob_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------
// A11 — parked-write budget: budget fraction over block size, shipped floor
// ---------------------------------------------------------------------------

/// The flat 256-buffer parked cap becomes `max(256, budget/16 ÷
/// block_size)` buffers' worth: parked write buffers are RAM the R5
/// authority already gauges and sheds (`parked_full_buffer_bytes`), so
/// the admission cap scales with the machine; floor 256 = the shipped
/// posture. `SQUEEZEFS_PARKED_BUFFERS` is the absolute A0 lever.
#[test]
fn parked_cap_derives_from_budget_over_block_size() {
    use fuse_client::resolve_parked_cap_buffers;
    // Field: 176 GiB/16 = 11 GiB ÷ 4 MiB = 2816 buffers.
    assert_eq!(
        resolve_parked_cap_buffers(None, FIELD_BUDGET, 4 * MIB),
        2816
    );
    // Floor shape: 2.8 GiB/16 = 179 MiB ÷ 4 MiB = 44 < 256 ⇒ floor.
    assert_eq!(resolve_parked_cap_buffers(None, FLOOR_BUDGET, 4 * MIB), 256);
    // Env verbatim (the A0 lever: 256 restores the old law).
    assert_eq!(
        resolve_parked_cap_buffers(Some("256"), FIELD_BUDGET, 4 * MIB),
        256
    );
    // Garbage falls through; block_size 0 is defensive-floored.
    assert_eq!(
        resolve_parked_cap_buffers(Some("piles"), FLOOR_BUDGET, 4 * MIB),
        256
    );
    assert_eq!(resolve_parked_cap_buffers(None, FIELD_BUDGET, 0), 2816);
}

// ---------------------------------------------------------------------------
// A9 — IPC per-session arena default: population-derived cap fraction
// ---------------------------------------------------------------------------

/// The derived IPC session POPULATION the admission cap is sized to
/// hold: `max(128, cpus × 8)` (`mem_budget::ipc_session_population_target`
/// — the 2026-08-04 squeeze-test population fix; user directive
/// verbatim: "we need to see how we make that some derived value from
/// the system size (cpu/memory/threads) something so it's not a hard
/// coded value").
///
/// - `cpus × 8` is the matched-inflight client-fleet slope — one shim
///   session per client process, and the EXA fairness law runs psync
///   fleets at `njobs × qd` with qd = 8 (the canon dims), so a box's
///   honest concurrent client population scales with its cores. ×8 is
///   the measured shape that refused, TWICE (two battery runs on the
///   176 GiB cluster): 32 cpus → 256 fio processes against the
///   ~128-session budget the /128 arena implied —
///   `ipc_admission_refusals` ~125/pass, ~half the fleet silently on
///   the kernel lane, engagement 0.697 ⇒ INVALID row — with the per-uid
///   cap ALREADY derived (it correctly read ~131; the ARENA SIZE was
///   the binding constraint).
/// - floor 128 = the population the retired /128 fraction implied
///   (never-regress-below-shipped, the `Q_DEPTH_FLOOR` house law): on
///   any box where `cpus × 8 < 128` the arena arithmetic stays
///   byte-identical to the shipped cap/128.
#[test]
fn ipc_session_population_target_derives_from_cores() {
    use mem_budget::ipc_session_population_target;
    // Field shape: the 32-CPU cluster client ⇒ 256 — the refused
    // battery's exact process count (njobs 32 × qd 8).
    assert_eq!(ipc_session_population_target(32), 256);
    // Floor region: ≤ 16 cpus keep the /128 population exactly (the
    // never-regress pin — small boxes keep today's arithmetic).
    assert_eq!(ipc_session_population_target(1), 128);
    assert_eq!(ipc_session_population_target(8), 128);
    assert_eq!(ipc_session_population_target(16), 128);
    // Above the floor the slope is linear in cores.
    assert_eq!(ipc_session_population_target(64), 512);
    assert_eq!(ipc_session_population_target(192), 1536);
}

/// The flat 64 MiB `SQUEEZEFS_IPC_ARENA_MB` default becomes
/// `max(64 MiB, dma_align_down(cap / population_target(cpus)))`: the
/// fraction sizes a default arena so the admission cap holds the
/// POPULATION TARGET above — the retired /128 literal was a legacy
/// "per-uid cap 64 × 2 safety" population ASSUMPTION, not a derivation
/// (see `ipc_session_population_target_derives_from_cores` for the
/// field conviction it starved) — and these are the SAME two numbers
/// the A12 per-uid session cap derives from, so a full per-uid
/// population of default-size arenas fills the cap exactly; alignment
/// is the SLOT-SLAB DMA
/// law (slots × 4 KiB = 4 MiB — also a PMD multiple, so the THP collapse
/// law `map_shared_pmd_aligned` still holds); floor 64 MiB = the shipped
/// posture. Env stays absolute-verbatim (MiB). `cpus` is the PROCESS
/// mask (`crate::cpu::process_parallelism`) at the one production call
/// site — never the calling thread's `available_parallelism()` (the
/// Hang-1 pinned-first-toucher sizing poison; the mount path runs on
/// core-pinned tokio workers).
///
/// The 4 MiB (not 2 MiB) alignment is the 2026-08-04 cluster bounce
/// regression's fix: an ODD 2 MiB-multiple arena makes the client slab
/// `arena/1024 ≡ 2048 (mod 4096)`, so every odd slot's `slot × slab`
/// arena offset fails `ipc_direct`'s 4 KiB DMA screen — EXACTLY half of
/// all round-robin direct-drive reads bounced through the pooled-copy
/// path on the fabric venue (randread-shim −15.4 % vs kernel; bounce
/// rate 50.006 % measured, expected 0 per
/// `.benchmarks/2026-07-26-ipc-direct-drive.md`).
#[test]
fn ipc_arena_default_derives_from_admission_cap() {
    use mem_budget::resolve_ipc_arena_bytes;
    // THE FIELD SHAPE (red vs the /128): cap = 22 GiB, 32 cpus ⇒
    // population 256 ⇒ 88 MiB per session (4 MiB-aligned; 256 × 88 MiB
    // = 22 GiB — the whole matched-inflight battery fits the admission
    // cap, where the /128's 176 MiB admitted only half of it).
    assert_eq!(
        resolve_ipc_arena_bytes(None, mem_budget::ipc_arena_cap(FIELD_BUDGET), 32),
        88 * MIB,
        "the 256-process battery must fit: cap / population(32 cpus) = 88 MiB"
    );
    // Small-box invariance (the never-regress pin): the population
    // floors at 128 binds, so any box with cpus × 8 < 128 keeps
    // today's cap/128 arithmetic byte-identical — same cap, 8 cpus.
    assert_eq!(
        resolve_ipc_arena_bytes(None, mem_budget::ipc_arena_cap(FIELD_BUDGET), 8),
        176 * MIB,
        "≤ 16-CPU boxes keep the shipped cap/128 value exactly"
    );
    // Floor shape: cap = 358 MiB ⇒ 2.8 MiB < 64 MiB ⇒ shipped floor.
    assert_eq!(
        resolve_ipc_arena_bytes(None, mem_budget::ipc_arena_cap(FLOOR_BUDGET), 2),
        64 * MIB
    );
    // Floor-vs-population interaction: a many-core box whose cap cannot
    // hold its population target at 64 MiB each still floors at the
    // shipped 64 MiB (the floor is the last word; the admission cap —
    // not a smaller arena — is what then bounds the population).
    assert_eq!(
        resolve_ipc_arena_bytes(None, mem_budget::ipc_arena_cap(FIELD_BUDGET), 512),
        64 * MIB,
        "22 GiB / 4096-session target < the shipped floor ⇒ 64 MiB wins"
    );
    // DMA alignment: a cap that derives to a non-4 MiB multiple rounds
    // DOWN (never over the cap fraction) — at the population floor…
    assert_eq!(resolve_ipc_arena_bytes(None, 129 * 128 * MIB, 8), 128 * MIB);
    // …and above it (the same shape scaled to the 32-CPU population).
    assert_eq!(
        resolve_ipc_arena_bytes(None, 129 * 256 * MIB, 32),
        128 * MIB
    );
    // THE BOUNCE SHAPE: cap/population = 90 MiB — an odd 2 MiB multiple.
    // The pre-fix PMD round-down kept 90 MiB (slab 92,160 B ≡ 2048 mod
    // 4096 ⇒ 50 % of slots DMA-ineligible); the DMA law rounds to
    // 88 MiB. Pinned at the population floor AND at the 32-CPU slope.
    assert_eq!(
        resolve_ipc_arena_bytes(None, 90 * 128 * MIB, 8),
        88 * MIB,
        "an odd 2 MiB-multiple arena is the 50 %-bounce shape — the \
         derivation must round to the slot-slab DMA alignment"
    );
    assert_eq!(resolve_ipc_arena_bytes(None, 90 * 256 * MIB, 32), 88 * MIB);
    // The derivation-wide invariant: every derived arena is slot-slab
    // DMA-aligned (slots × 4 KiB) at every core count.
    let align = u64::from(squeezefs_ipc::layout::Geometry::default_v1().slots) * 4096;
    assert_eq!(
        align,
        4 * MIB,
        "geometry drift — re-derive the arena alignment"
    );
    for cap_mib in [1_u64, 300, 8192, 11_520, 22_528, 90 * 128, 129 * 128] {
        for cpus in [1_usize, 2, 8, 16, 32, 64, 192, 512] {
            let got = resolve_ipc_arena_bytes(None, cap_mib * MIB, cpus);
            assert_eq!(
                got % align,
                0,
                "derived arena {got} for cap {cap_mib} MiB / {cpus} cpus is \
                 not slot-slab DMA-aligned"
            );
        }
    }
    // Env verbatim (MiB), incl. the A0 lever, at any core count.
    assert_eq!(resolve_ipc_arena_bytes(Some("64"), 22 * GIB, 32), 64 * MIB);
    assert_eq!(
        resolve_ipc_arena_bytes(Some("junk"), FLOOR_BUDGET, 2),
        64 * MIB
    );
    // The field capture's measured operator escape stays verbatim
    // (SQUEEZEFS_IPC_ARENA_MB=80: 256 × 80 MiB = 20 GiB ≤ cap) — the
    // derivation makes it unnecessary, never overrides it.
    assert_eq!(resolve_ipc_arena_bytes(Some("80"), 22 * GIB, 32), 80 * MIB);
    // An EXPLICIT odd env value stays verbatim (explicit-wins law) — the
    // CLIENT slab law (`Geometry::slot_slab`) is what keeps its slots
    // DMA-eligible; see `slot_slab_is_dma_aligned` below.
    assert_eq!(resolve_ipc_arena_bytes(Some("90"), 22 * GIB, 32), 90 * MIB);
}

/// The ONE slab law (`Geometry::slot_slab` — the client-side face of the
/// same bounce fix): `arena / slots`, capped at `max_op_bytes`, floored
/// to the 4 KiB DMA LBA whenever it is at least one LBA — so `slot ×
/// slab` is page-aligned for EVERY slot regardless of the arena size an
/// explicit env override picked. Sub-LBA slabs (toy test geometries)
/// pass through verbatim: they can never DMA-align and always ride the
/// bounce path by design.
#[test]
fn slot_slab_is_dma_aligned() {
    use squeezefs_ipc::layout::Geometry;
    let mk = |arena_mib: u64| {
        let mut g = Geometry::default_v1();
        g.arena_bytes = arena_mib * MIB;
        g
    };
    // Shipped floor: 64 MiB / 1024 = 64 KiB — aligned, unchanged.
    assert_eq!(mk(64).slot_slab(), 64 * 1024);
    // The cluster shape's POPULATION-derived arena (A9 field shape:
    // 22 GiB cap / 256 sessions = 88 MiB): slab = 88 MiB / 1024 =
    // 90,112 B — a 4 KiB multiple, so every slot stays DMA-eligible.
    assert_eq!(mk(88).slot_slab(), 88 * 1024);
    assert_eq!(mk(88).slot_slab() % 4096, 0);
    // THE BOUNCE SHAPE: 90 MiB / 1024 = 92,160 B (2048 mod 4096) —
    // floored to 90,112 B (a 4 KiB multiple).
    assert_eq!(
        mk(90).slot_slab(),
        90 * 1024 * 1024 / 1024 / 4096 * 4096,
        "an odd 2 MiB-multiple arena's slab must floor to the DMA LBA"
    );
    assert_eq!(mk(90).slot_slab() % 4096, 0);
    // max_op cap still applies (giant arenas).
    let mut big = Geometry::default_v1();
    big.arena_bytes = 8 * 1024 * MIB;
    assert_eq!(big.slot_slab(), u64::from(big.max_op_bytes));
    // Sub-LBA slab (toy geometry): verbatim, never zero.
    let mut tiny = Geometry::default_v1();
    tiny.arena_bytes = MIB;
    assert_eq!(tiny.slot_slab(), MIB / 1024);
}

// ---------------------------------------------------------------------------
// A10 — uring_fs worker pool: cores fraction, shipped floor
// ---------------------------------------------------------------------------

/// `clamp(nproc, 4, 8)` becomes `clamp(cpus/4, 4, 64)`: cpus/4 is the
/// measured drain-thread slope (the ingest-economy `il_sessions_default`
/// precedent applied to the sibling file-I/O pool); floor 4 = the
/// shipped floor; ceiling 64 = the existing env sanity clamp. Field
/// 32-CPU box: 8 workers — byte-identical to the old law. Sizing must
/// use the PROCESS mask (`crate::cpu::process_parallelism`), never the
/// calling thread's (the Hang-1 pinned-first-toucher poison — the old
/// site read `available_parallelism()` from whatever thread touched the
/// Lazy first).
#[test]
fn uring_fs_workers_derive_from_process_cores() {
    use squeezefs::uring_fs::resolve_worker_count;
    assert_eq!(resolve_worker_count(None, 32), 8, "field shape: unchanged");
    assert_eq!(resolve_worker_count(None, 2), 4, "floor shape");
    assert_eq!(resolve_worker_count(None, 64), 16);
    assert_eq!(resolve_worker_count(None, 512), 64, "env-clamp parity cap");
    assert_eq!(resolve_worker_count(Some("2"), 32), 2, "env verbatim");
    assert_eq!(resolve_worker_count(Some("999"), 32), 64, "env clamp");
    assert_eq!(resolve_worker_count(Some("junk"), 32), 8);
}

// ---------------------------------------------------------------------------
// A12 — IPC per-uid session cap: session-budget derived, shipped floor
// ---------------------------------------------------------------------------

/// The bare `per_uid_session_cap: 64` becomes `clamp(arena_cap_bytes /
/// arena_bytes, 64, 4096)` — the session population the R5 admission cap
/// can actually hold (2026-08-04 field conviction: a matched-inflight
/// 256-process fio battery on the squeeze-test cluster hit the literal —
/// `ipc_admission_refusals` 125, ~192 of 256 jobs silently on the kernel
/// lane, engagement 0.695 ⇒ INVALID row — while the box's own session
/// budget, cap ≈ 22.5 GiB ÷ 176 MiB arenas ≈ 131, admitted twice the
/// cap). With the A9 population fix the derived arena divides the cap
/// by `ipc_session_population_target(cpus)`, so the quotient here ≈
/// that target (256 on the cluster shape) — the two derivations are
/// COHERENT by construction: same cap, same arena, no third number.
/// Per-uid is a DoS tripwire, not an inter-uid fairness device
/// (VAL-7d: single-tenant by declaration), so the budget IS the bound;
/// floor 64 = the shipped posture (never-regress-below-shipped); rail
/// 4096 = the ctl-thread exhaustion rail (`ctl_conn_cap_from` — every
/// admitted session holds a ctl connection = one OS thread). No env
/// knob: the cap was never operator-tunable, and the budget knobs it
/// derives from (`SQUEEZEFS_IPC_MEM_{PCT,MAX}`, `SQUEEZEFS_IPC_ARENA_MB`)
/// remain the operator levers.
#[test]
fn ipc_per_uid_session_cap_derives_from_session_budget() {
    use mem_budget::{ipc_arena_cap, ipc_per_uid_session_cap, resolve_ipc_arena_bytes};
    // THE CLUSTER SHAPE (the two INVALID battery rows' box): cap
    // 22 GiB, 32 cpus ⇒ population target 256 ⇒ derived arena 88 MiB ⇒
    // uid cap 256 — the canonical pair divides exactly, so a full
    // per-uid population of default arenas fills the admission cap
    // exactly and the WHOLE 256-process matched-inflight battery binds.
    let field_cap = ipc_arena_cap(FIELD_BUDGET);
    let field_arena = resolve_ipc_arena_bytes(None, field_cap, 32);
    assert_eq!(field_arena, 88 * MIB, "A9 anchor drifted — re-derive A12");
    assert_eq!(
        ipc_per_uid_session_cap(field_cap, field_arena),
        256,
        "cluster shape: cap/arena must equal the population target — the \
         256-process battery binds in full"
    );
    // Population-floor coherence (the never-regress pin): a ≤ 16-CPU
    // box keeps the shipped /128 pair — arena 176 MiB, uid cap 128
    // (double the bare-64 literal this cap replaced; the field
    // capture's measured pair ≈ 22.5 GiB / 176 MiB ⇒ ≈ 131 was the
    // same class).
    let small_arena = resolve_ipc_arena_bytes(None, field_cap, 8);
    assert_eq!(small_arena, 176 * MIB, "small-box A9 anchor drifted");
    assert_eq!(
        ipc_per_uid_session_cap(field_cap, small_arena),
        128,
        "population floor: the session budget holds 128 default arenas — \
         the bare 64 refused half of them"
    );
    // Alignment round-down RAISES the quotient past the target (the A9
    // bounce shape at the population floor): cap 11.25 GiB ⇒ arena
    // 88 MiB ⇒ 130.
    assert_eq!(
        ipc_per_uid_session_cap(
            90 * 128 * MIB,
            resolve_ipc_arena_bytes(None, 90 * 128 * MIB, 8)
        ),
        130
    );
    // Floor shape: cap ≈ 358 MiB, arena at the 64 MiB shipped floor ⇒
    // the budget holds 5 sessions — the floor keeps the shipped 64
    // (never-regress-below-shipped; the budget admission at
    // `arena_cap_bytes` still refuses the 6th arena first).
    let floor_cap = ipc_arena_cap(FLOOR_BUDGET);
    assert_eq!(
        ipc_per_uid_session_cap(floor_cap, resolve_ipc_arena_bytes(None, floor_cap, 2)),
        64,
        "floor shape: never regress below the shipped 64"
    );
    // Rail shape: an enormous cap (env-set SQUEEZEFS_IPC_MEM_MAX / tiny
    // SQUEEZEFS_IPC_ARENA_MB) rails at 4096 — the daemon session store
    // is an unbounded map and the shim's SESSION_REGISTRY_SLOTS (32)
    // bounds a client PROCESS, not the uid population across processes,
    // so the ctl-thread rail is the binding structural bound.
    assert_eq!(ipc_per_uid_session_cap(u64::MAX, 64 * MIB), 4096);
    assert_eq!(ipc_per_uid_session_cap(1024 * GIB, MIB), 4096);
    // Defensive: a zero arena never divides-by-zero — the divisor floors
    // at 1 (mirroring `ctl_conn_cap_from`'s `session_footprint.max(1)`),
    // so nonsense geometry rails rather than panicking.
    assert_eq!(ipc_per_uid_session_cap(100 * MIB, 0), 4096);
}

/// Coherence: growing the per-session arena at a fixed admission cap can
/// only LOWER (never raise) the session population the budget holds —
/// the cap must be monotone non-increasing in `arena_bytes`.
#[test]
fn ipc_per_uid_session_cap_monotone_in_arena() {
    use mem_budget::ipc_per_uid_session_cap;
    let cap = 22 * GIB;
    let mut last = usize::MAX;
    for arena_mib in [4_u64, 16, 64, 88, 176, 256, 1024, 4096, 22 * 1024] {
        let got = ipc_per_uid_session_cap(cap, arena_mib * MIB);
        assert!(
            got <= last,
            "fixed cap {cap}: growing the arena to {arena_mib} MiB RAISED \
             the session cap ({got} > {last})"
        );
        last = got;
    }
}

// ---------------------------------------------------------------------------
// Derivation-debt audit (2026-08-04; user directive verbatim: "nothing
// should be a hard coded set number maybe a percentage or calculation but
// never just 8..."): every remaining bare number either gets ONE
// definition site (tied here, drift-is-red) or a documented reason on the
// line. Evidence note: `.benchmarks/2026-08-04-derivation-debt-audit.md`.
// ---------------------------------------------------------------------------

/// DEBT-1 — the population target's ×8 is the EXA canon iodepth, and the
/// canon now has ONE definition ([`mem_budget::EXA_CANON_QD`]): the fio
/// canon files (`tests/fio/run_fio_row.sh` runner default + every
/// `exa_client_perf.sh` battery row) must carry the SAME qd, so the "8"
/// in the matched-inflight slope, the runner, and the battery can never
/// drift apart silently. The canon itself is an external instrument's
/// dims (shape parity with the DDN exa-client validation kit — a fact
/// about the instrument, not tuning).
#[test]
fn exa_canon_qd_has_one_definition_tied_to_the_fio_canon() {
    use mem_budget::{ipc_session_population_target, EXA_CANON_QD};
    // The canon pin (an instrument fact — changing it is a canon change,
    // which must be a conscious act across the fio files AND this const).
    assert_eq!(EXA_CANON_QD, 8, "the EXA canon iodepth changed — re-derive");
    // The population slope IS the canon: cpus × qd above the floor.
    assert_eq!(
        ipc_session_population_target(32),
        32 * EXA_CANON_QD,
        "the matched-inflight slope must be cpus × EXA_CANON_QD"
    );
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // The runner's default iodepth (tests/fio/run_fio_row.sh).
    let runner = std::fs::read_to_string(root.join("tests/fio/run_fio_row.sh"))
        .expect("tests/fio/run_fio_row.sh must exist — the fio canon moved?");
    let runner_qd = runner
        .lines()
        .find_map(|l| {
            let (_, rest) = l.split_once("IODEPTH=\"")?;
            rest.split_once('"')?.0.parse::<u64>().ok()
        })
        .expect("run_fio_row.sh carries no IODEPTH=\"N\" default");
    assert_eq!(
        runner_qd, EXA_CANON_QD,
        "run_fio_row.sh's IODEPTH default drifted from the canon const"
    );
    // Every exa battery row (exa_client_perf.sh: name|job|class|bs|qd|…).
    let battery = std::fs::read_to_string(root.join("tests/fio/exa_client_perf.sh"))
        .expect("tests/fio/exa_client_perf.sh must exist — the fio canon moved?");
    let mut rows = 0;
    for line in battery.lines() {
        let mut f = line.split('|');
        let name = f.next().unwrap_or("");
        if !matches!(name, "write_bw" | "read_bw" | "randwrite" | "randread") {
            continue;
        }
        let qd: u64 = f
            .nth(3)
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("battery row {name} has no parseable qd column"));
        assert_eq!(
            qd, EXA_CANON_QD,
            "battery row {name} drifted from the canon"
        );
        rows += 1;
    }
    assert_eq!(
        rows, 4,
        "the exa battery no longer carries its four canon rows"
    );
}

/// DEBT-2 — the rewrite-epoch idle-close horizon really derives from the
/// layout metadata cache's time-to-idle: both now read ONE constant
/// (`routing::METADATA_CACHE_TTI_SECS`) instead of the pre-audit shape
/// (the horizon spelled `300_000 / 10` while the cache builder carried
/// its own bare `Duration::from_secs(300)` — a claimed derivation whose
/// input had no definition site).
#[test]
fn metadata_cache_tti_has_one_definition_and_derives_the_idle_horizon() {
    use squeezefs::routing;
    // The horizon pin: a TTI change is a cache-posture change (time
    // horizon, not a resource cap) and must be conscious.
    assert_eq!(routing::METADATA_CACHE_TTI_SECS, 300);
    // The derivation (KD-1.6): idle-close horizon = TTI ÷ 10.
    assert_eq!(
        routing::EPOCH_IDLE_HORIZON_MS,
        routing::METADATA_CACHE_TTI_SECS * 1000 / 10
    );
    assert_eq!(routing::EPOCH_IDLE_HORIZON_MS, 30_000);
}

/// DEBT-3 — the read path's 256 KiB size-class boundary has ONE
/// definition (`routing::READ_SIZE_CLASS_BOUNDARY_BYTES`): the R1b
/// always-admit arm, the R4 RAM-LRU-vs-hot-tier split, and the read-lane
/// hold's participation floor are the SAME boundary (design-read-path
/// §5.3/§5.4; the hold's doc always said "mirrored" — mirroring by
/// retyping the literal is exactly the drift class this audit deletes).
#[test]
fn read_size_class_boundary_has_one_definition() {
    use squeezefs::{read_lane, routing};
    assert_eq!(routing::READ_SIZE_CLASS_BOUNDARY_BYTES, 256 * 1024);
    assert_eq!(
        read_lane::READ_LANE_MIN_FILL_BYTES,
        routing::READ_SIZE_CLASS_BOUNDARY_BYTES,
        "the hold's participation floor must BE the read size-class boundary"
    );
}

/// DEBT-4 — the zcrx lane's derived geometry is a PURE function of the
/// process core count (`probe::lane_geometry`), pinned on the canonical
/// shapes: queues = `clamp(cpus/8, 1, LANE_IO_QUEUES_MAX)` (the §8
/// slope; the NIC-derived `nic_queues/4` ceiling applies at steering),
/// depth = `clamp(cpus × 2, 4, 64)` (§8's `derived_inflight` clamp; the
/// slope is the documented INTERIM stand-in for the unbuilt BDP probe,
/// and CAP.MQES still clamps at connect).
#[test]
fn zcrx_lane_geometry_is_pinned_on_canonical_shapes() {
    use squeezefs::zcrx_lane::probe::{lane_geometry, LANE_IO_QUEUES_MAX};
    assert_eq!(lane_geometry(32), (4, 64), "field shape: 32-CPU client");
    assert_eq!(lane_geometry(2), (1, 4), "floor shape: physical minima");
    assert_eq!(lane_geometry(1), (1, 4));
    assert_eq!(lane_geometry(8), (1, 16));
    assert_eq!(
        lane_geometry(192),
        (LANE_IO_QUEUES_MAX, 64),
        "big boxes rail at the pre-steering want bound"
    );
    assert_eq!(
        LANE_IO_QUEUES_MAX, 8,
        "want-bound drift — re-derive the area math"
    );
}

/// DEBT-5 — the lane's per-command transfer cap IS the FUSE transport's
/// payload face: `LANE_MAX_XFER_CAP_BYTES` is DEFINED from fuse3's
/// [`PAYLOAD_BASE`] (yesterday's shipped 1 MiB ent — the largest single
/// destination a lane dest-serve fills in one command today), so the two
/// 1 MiB literals can never drift apart. The value pin makes a fuse3
/// payload-floor change show up HERE as a conscious lane re-derivation.
#[test]
fn zcrx_lane_xfer_cap_is_the_transport_payload_face() {
    use fuse3::raw::connection::fuse_over_uring::PAYLOAD_BASE;
    use squeezefs::zcrx_lane::probe::LANE_MAX_XFER_CAP_BYTES;
    assert_eq!(LANE_MAX_XFER_CAP_BYTES as usize, PAYLOAD_BASE);
    assert_eq!(LANE_MAX_XFER_CAP_BYTES, 1024 * 1024, "Z2-benched MDTS face");
}

/// The zcrx ENGAGEMENT geometry (2026-08-06 campaign — phase 2 of the
/// read copy-elimination program) is DERIVED end-to-end; drift-is-red
/// on the canonical shapes:
/// * eligible pool = `clamp(devices_via_nic, channels/4, channels/2)` —
///   the census input widens the flat §8 /4 pool exactly as far as the
///   fabric-device breadth demands (floor = the standing ¼ posture,
///   ceiling = the kernel path keeps ≥ half the NIC's RSS width);
/// * admission = the FULL fill window `depth × max_xfer` (the retired
///   /2's implicit slack budget moved to the AREA, derived from the
///   MTU/chunk burst-occupancy arithmetic — `delivery_slack_bytes`);
/// * area = window + slack + ring-standing (rounds 6–7 unchanged).
#[test]
fn zcrx_engagement_geometry_derives_on_canonical_shapes() {
    use squeezefs::zcrx_lane::area;
    use squeezefs::zcrx_lane::steering::lane_eligible_queues;
    // Pool width: (channels, devices) → width.
    for (ch, dev, width) in [
        (32u32, 10usize, 10u32), // the field rail: 10 devices, all covered
        (32, 5, 8),              // two-rail split: floor binds
        (32, 0, 8),              // failed census: the sole-device posture
        (32, 100, 16),           // ceiling: RSS keeps ≥ half the NIC
        (64, 20, 20),
        (3, 10, 0), // too narrow: never dedicates ZC queues
    ] {
        let p = lane_eligible_queues(ch, dev);
        assert_eq!(p.end - p.start, width, "pool width at ({ch}, {dev})");
        assert_eq!(p.end, ch, "the pool stays the HIGHEST-indexed slice");
    }
    // Admission is the whole window; slack derives from burst occupancy.
    let window = area::area_bytes_per_queue(64, 1 << 20); // the field queue
    assert_eq!(
        area::admission_permits(window as usize, 4096) as u64,
        window / 4096
    );
    assert_eq!(
        area::delivery_slack_bytes(window, Some(9000), 4096),
        24 << 20
    );
    assert_eq!(area::delivery_slack_bytes(window, None, 4096), window);
}

/// Ingress-queue-spread lever 2 (2026-08-05, hard-constant ruling): the
/// drain-group width is DERIVED — `drain_group_width(node_possible_cpus)`
/// = the house `cpus/4` drain-parallelism SLOPE (the
/// `il_sessions_default` lineage; the ipc drain-lane pair moved to its
/// own class-measured 3×cpus/8 slope in the 2026-08-06 width re-grade,
/// this transport width keeps its own bracket-validated cpus/4) on the
/// NODE's possible-CPU span, floor 1 (physical minimum: a context owns at
/// least one queue). NEVER a frozen bracket winner: the counted 2026-08-05
/// bracket validated the slope at the 32-possible shape (32/4 = 8 — the
/// winning width, byte-identical to the derived value, so the A/B rows
/// carry over verbatim), and the field ladder re-grades the SLOPE, not a
/// constant. Drift-is-red on the canonical shapes (the dd_shards pattern).
#[test]
fn fuse_drain_group_width_is_the_cpus_over_4_slope_per_node() {
    use fuse3::raw::connection::fuse_over_uring::{drain_group_plan, drain_group_width};
    use fuse3::raw::connection::kmbuf::TransportBufferMode;
    // The slope on canonical shapes: 32-possible/1-node ⇒ 8 (the
    // bracket-validated shape); a 96-possible/2-node box ⇒ 12 per node;
    // a 4-CPU box ⇒ 1 (= today's per-queue posture — the floor).
    assert_eq!(drain_group_width(32), 8, "32-possible node: cpus/4 = 8");
    assert_eq!(drain_group_width(48), 12, "48-possible node: cpus/4 = 12");
    assert_eq!(drain_group_width(4), 1, "4-possible node: cpus/4 = 1");
    assert_eq!(
        drain_group_width(1),
        1,
        "floor 1 — a context owns ≥ 1 queue"
    );
    // The plan applies the slope PER NODE MEMBERSHIP SET: 96 possible
    // over 2 contiguous-block nodes of 48 ⇒ 4 contexts of width 12 per
    // node, never spanning a node.
    let plan = drain_group_plan(
        96,
        TransportBufferMode::UserEnts,
        true,
        |c| Some(c / 48),
        None,
    );
    assert_eq!(plan.len(), 8, "2 nodes × 4 contexts");
    assert!(
        plan.iter().all(|g| g.len() == 12),
        "width = node population / 4 on 48-CPU nodes"
    );
    assert!(
        plan.iter()
            .all(|g| g.iter().all(|&q| (q as usize / 48) == (g[0] as usize / 48))),
        "groups never span a node"
    );
    // THE FIELD SHAPE (2026-08-05 disengagement finding): INTERLEAVED
    // node numbering — node0 = even qids, node1 = odd (squeeze-test's
    // BIOS round-robin numbering). Membership grouping derives 8 groups
    // of 4 on 32-possible/2-node; the contiguous-run plan derived 32
    // singletons here (the A0 posture, structurally disengaged —
    // portable-by-default covers arbitrary NUMBERINGS, not just
    // arbitrary domain counts).
    let plan = drain_group_plan(
        32,
        TransportBufferMode::UserEnts,
        true,
        |c| Some(c % 2),
        None,
    );
    assert_eq!(
        plan.len(),
        8,
        "interleaved 2×16: 8 groups, never 32 singletons"
    );
    assert!(plan.iter().all(|g| g.len() == 4), "width 16/4 = 4 per node");
    assert!(
        plan.iter().all(|g| g.iter().all(|&q| q % 2 == g[0] % 2)),
        "groups never span a node on the interleaved numbering"
    );
    // Whole-node width must never be the DEFAULT (the counted bracket's
    // falsifier: one context per node collapsed −15 % on the
    // single-thread drain ceiling).
    let plan = drain_group_plan(32, TransportBufferMode::UserEnts, true, |_c| Some(0), None);
    assert!(
        plan.iter().all(|g| g.len() == 8),
        "32-possible single node: 4 contexts of 8 — the bracket-validated shape"
    );
}

// ---------------------------------------------------------------------------
// Direct-drive width re-grade (2026-08-06): the drain-LANE 3×cpus/8 slope
// ---------------------------------------------------------------------------

/// The counted field width sweep (squeeze-test, 32 CPUs / 2 nodes, il
/// rand-4k 32×qd32, 3×30 s rows/width, W8 brackets both ends, engagement
/// exact — `.benchmarks/2026-08-06-dd-width-slope.md`) found a genuine
/// INTERIOR optimum at W12 on the 32-CPU shape: W8 622–636k → W12
/// 695–700k (+10.4 %, clat down) → W16 675–690k → W24 616–626k
/// (regression). The derivation is the LANE-PAIR budget, never the point:
/// one lane = 2 OS threads (svc submitter + dd reaper), so
/// `il_drain_lanes_default(cpus)` = clamp(3×cpus/8, 2, 64) puts the lane
/// thread population 2W at ¾ of the core budget — the sweep's one point
/// past 1.0× (W24 = 1.5×) is its one regression, verifying the
/// oversubscription failure mode in the same data. Floor 2 = the pre-L4-8
/// single-consumer plateau (never-regress: ⌊3c/8⌋ ≥ ⌊c/4⌋ pointwise, so
/// no box shape derives below the previously shipped width); rail 64 =
/// the explicit-lever clamp parity (a default must be expressible as an
/// explicit setting; engages only at cpus ≥ 174 — harmless where absent).
/// The shim session default and the fuse3 drain-group width keep their
/// own measured cpus/4 slopes — this class is the DAEMON drain lane only.
#[test]
fn il_drain_lane_width_is_the_three_eighths_lane_pair_slope() {
    use squeezefs_ipc::sizing::{il_drain_lanes_default, il_sessions_default};
    // The field shape: the counted sweep's interior optimum.
    assert_eq!(il_drain_lanes_default(32), 12, "32-CPU field box ⇒ 12");
    // The floor shape (the sweep's canonical 2-CPU box).
    assert_eq!(il_drain_lanes_default(2), 2, "floor: pre-L4-8 plateau");
    assert_eq!(il_drain_lanes_default(4), 2, "3×4/8 = 1 ⇒ floor 2");
    // Big-box slope points (the slope is the claim, not the 32-CPU point).
    assert_eq!(il_drain_lanes_default(64), 24);
    assert_eq!(il_drain_lanes_default(96), 36);
    // The env-rail parity ceiling.
    assert_eq!(il_drain_lanes_default(256), 64, "railed at the lever clamp");
    assert_eq!(il_drain_lanes_default(usize::MAX), 64, "no overflow");
    // Dominance / never-regress: at every machine size the lane width
    // covers the shim's session default (every default session keeps its
    // own drain thread — the ingest-economy topology preserved by
    // subsumption) and never derives below the previously shipped cpus/4.
    for cpus in 1..=512usize {
        assert!(
            il_drain_lanes_default(cpus) >= il_sessions_default(cpus),
            "dominance broken at cpus={cpus}"
        );
    }
}

// ---------------------------------------------------------------------------
// KD-MW-14 / design-full-multi-writer §5.6 — the fleet-share root divisor
// (`SQUEEZEFS_FLEET_SHARE`): ONE divisor at the ROOT INPUTS of the derived-
// sizing tree — the memory-budget root and the sizing CPU-count root — so
// every downstream formula scales untouched. Derived tier ONLY (absolute >
// pct > derived precedence unchanged); floors are NEVER divided; the
// kernel-mandated-geometry exemption class (the FUSE-over-uring queue
// COUNT) is pinned below. Shares round UP (the rounding doctrine); never
// auto-detected — operator/rig-set only.
// ---------------------------------------------------------------------------

/// A machine root chosen for exact arithmetic (320 GiB divides by 4
/// through both the 70 % RAM law and every downstream fraction), so the
/// share=4 rows can assert exact quartering, not just formula equality.
const FLEET_RAM: u64 = 320 * GIB;

/// The memory root: the share divides the SYSTEM inputs (RAM, the cgroup
/// cage) BEFORE the existing resolution laws (×0.7 / ×0.8) — never the
/// resolved budget after an explicit knob. Explicit tiers (flag, absolute
/// env) win verbatim, undivided. `fleet_shared_root` is the one rounding
/// site: UP (`div_ceil`) per the rounding doctrine.
#[test]
fn fleet_share_divides_the_memory_root_derived_tier_only() {
    use mem_budget::{fleet_shared_root, resolve_budget_from_shared};
    // Derived RAM leg: root divided BEFORE the 70 % law.
    assert_eq!(
        resolve_budget_from_shared(None, None, None, FLEET_RAM, 4),
        56 * GIB,
        "320 GiB ÷ 4 = 80 GiB effective RAM ⇒ 70 % = 56 GiB"
    );
    // Cgroup leg: the cage is a system root too — divided BEFORE the ×0.8.
    assert_eq!(
        resolve_budget_from_shared(None, None, Some(10 * GIB), FLEET_RAM, 4),
        2 * GIB,
        "10 GiB cage ÷ 4 = 2.5 GiB ⇒ ×0.8 = 2 GiB"
    );
    // EXPLICIT tiers are never divided (absolute > pct > derived,
    // explicit-wins-verbatim — the ipc-cap precedent).
    assert_eq!(
        resolve_budget_from_shared(Some(2 * GIB), Some(3 * GIB), Some(8 * GIB), FLEET_RAM, 4),
        2 * GIB,
        "the --mem-budget flag wins verbatim at any share"
    );
    assert_eq!(
        resolve_budget_from_shared(None, Some(3 * GIB), Some(8 * GIB), FLEET_RAM, 4),
        3 * GIB,
        "SQUEEZEFS_MEM_BUDGET_MB (absolute env) wins verbatim at any share"
    );
    // The rounding doctrine: shares round UP where division allocates.
    assert_eq!(fleet_shared_root(10, 4), 3, "ceil(10/4) = 3, never 2");
    assert_eq!(fleet_shared_root(5, 1), 5, "share 1 is the identity");
    assert_eq!(
        fleet_shared_root(5, 0),
        5,
        "share 0 is defensive-identity (the registry refuses it at startup)"
    );
    assert_eq!(
        fleet_shared_root(u64::MAX, 2),
        u64::MAX / 2 + 1,
        "no overflow"
    );
}

/// The CPU root: `ceil(raw / share)`, never 0 — the effective count every
/// `crates/squeezefs-ipc/src/sizing.rs`-fed derivation reads.
#[test]
fn fleet_share_divides_the_cpu_root_rounding_up() {
    use squeezefs::cpu::effective_parallelism_from;
    assert_eq!(effective_parallelism_from(32, 4), 8, "the field box at N=4");
    assert_eq!(
        effective_parallelism_from(32, 5),
        7,
        "ceil(6.4) = 7 — round UP"
    );
    assert_eq!(effective_parallelism_from(33, 4), 9, "ceil(8.25) = 9");
    assert_eq!(effective_parallelism_from(2, 64), 1, "never 0 — floor 1");
    assert_eq!(effective_parallelism_from(1, 1), 1);
    assert_eq!(
        effective_parallelism_from(0, 4),
        1,
        "degenerate raw never 0"
    );
    for raw in [1usize, 2, 8, 32, 192] {
        assert_eq!(
            effective_parallelism_from(raw, 1),
            raw,
            "share 1 is the identity at every machine size"
        );
    }
}

/// The rung-3b red row: share=4 quarters EVERY divisible derived cap —
/// the derived tier minus the exemption class. Division happens at the
/// ROOT, so each downstream formula applied to the shared root must
/// produce exactly the quartered value wherever it is above its floor.
#[test]
fn fleet_share_quarters_every_divisible_derived_cap() {
    use mem_budget::resolve_budget_from_shared;
    use squeezefs::cpu::effective_parallelism_from;
    use squeezefs::meta_backend::kv::backend::resolve_node_cache_budget;
    use squeezefs::meta_backend::kv::checkpoint::resolve_max_dirty_nodes;
    use squeezefs::zcrx_lane::probe::lane_geometry;
    use squeezefs_ipc::sizing::il_drain_lanes_default;

    // The memory root and its downstream fractions.
    let whole = resolve_budget_from_shared(None, None, None, FLEET_RAM, 1);
    let shared = resolve_budget_from_shared(None, None, None, FLEET_RAM, 4);
    assert_eq!(whole, 224 * GIB);
    assert_eq!(shared, whole / 4, "the budget root itself quarters");
    // Transport payload-buffer cap (budget/8): 28 GiB → 7 GiB.
    assert_eq!(mem_budget::transport_buffer_cap(shared), 7 * GIB);
    assert_eq!(
        mem_budget::transport_buffer_cap(shared),
        mem_budget::transport_buffer_cap(whole) / 4
    );
    // IPC session-arena admission cap (budget/8): same quarter.
    assert_eq!(
        mem_budget::ipc_arena_cap(shared),
        mem_budget::ipc_arena_cap(whole) / 4
    );
    // Meta node cache (budget/16, floor 512 MiB): 14 GiB → 3.5 GiB.
    assert_eq!(
        resolve_node_cache_budget(shared, None, None),
        3 * GIB + 512 * MIB
    );
    assert_eq!(
        resolve_node_cache_budget(shared, None, None),
        resolve_node_cache_budget(whole, None, None) / 4
    );
    // Checkpoint dirty-node cap (budget/32 ÷ node): 28672 → 7168.
    assert_eq!(resolve_max_dirty_nodes(shared, 256 * 1024, None), 7168);
    assert_eq!(
        resolve_max_dirty_nodes(shared, 256 * 1024, None),
        resolve_max_dirty_nodes(whole, 256 * 1024, None) / 4
    );
    // Parked-write buffers (budget/16 ÷ block): 3584 → 896.
    assert_eq!(
        fuse_client::resolve_parked_cap_buffers(None, shared, 4 * MIB),
        896
    );
    assert_eq!(
        fuse_client::resolve_parked_cap_buffers(None, shared, 4 * MIB),
        fuse_client::resolve_parked_cap_buffers(None, whole, 4 * MIB) / 4
    );

    // The CPU root and its downstream slopes (the field 32-CPU box, N=4).
    let cpus = effective_parallelism_from(32, 4);
    assert_eq!(cpus, 8);
    // Drain-lane width (3×cpus/8): 12 → 3.
    assert_eq!(il_drain_lanes_default(cpus), il_drain_lanes_default(32) / 4);
    // NVMe submission fan-out (cpus ÷ devices): 16 → 4 on 2 devices.
    assert_eq!(
        squeezefs::nvme_dev::io_lanes_for(cpus, 2),
        squeezefs::nvme_dev::io_lanes_for(32, 2) / 4
    );
    // zcrx lane geometry: (4, 64) → (1, 16) — both axes scale.
    assert_eq!(lane_geometry(cpus), (1, 16));
    assert_eq!(lane_geometry(32), (4, 64));
}

/// Floors are NEVER divided: at every share the derived values hold their
/// physical-minimum / never-regress-below-shipped floors — silent
/// starvation below a floor is what the refusal (next test) exists to
/// prevent, never what a share produces.
#[test]
fn fleet_share_floors_hold_and_are_never_divided_at_every_share() {
    use fuse3::raw::connection::fuse_over_uring::{PAYLOAD_BASE, Q_DEPTH_FLOOR};
    use mem_budget::resolve_budget_from_shared;
    use squeezefs::cpu::effective_parallelism_from;
    use squeezefs::meta_backend::kv::backend::{
        resolve_commit_batch_txs, resolve_node_cache_budget,
    };
    use squeezefs::meta_backend::kv::checkpoint::resolve_max_dirty_nodes;
    use squeezefs::uring_fs::resolve_worker_count;
    use squeezefs::zcrx_lane::probe::lane_geometry;
    use squeezefs_ipc::sizing::{il_drain_lanes_default, il_sessions_default};

    // The floor box (4 GiB / 2 CPUs) at escalating shares.
    for share in [2usize, 4, 32, 4096] {
        let b = resolve_budget_from_shared(None, None, None, 4 * GIB, share);
        assert_eq!(
            resolve_node_cache_budget(b, None, None),
            512 * MIB,
            "node-cache floor holds undivided at share={share}"
        );
        assert_eq!(
            resolve_max_dirty_nodes(b, 256 * 1024, None),
            4096,
            "dirty-node floor holds at share={share}"
        );
        assert_eq!(
            fuse_client::resolve_parked_cap_buffers(None, b, 4 * MIB),
            256,
            "parked-buffer floor holds at share={share}"
        );
        let c = effective_parallelism_from(2, share);
        assert!(c >= 1, "the CPU root never divides to 0 at share={share}");
        assert_eq!(resolve_commit_batch_txs(None, c), 64, "M7 floor holds");
        assert_eq!(il_drain_lanes_default(c), 2, "drain-lane floor holds");
        assert_eq!(il_sessions_default(c), 2, "session floor holds");
        assert_eq!(resolve_worker_count(None, c), 4, "uring_fs floor holds");
        assert_eq!(
            mem_budget::ipc_session_population_target(c),
            128,
            "population floor holds"
        );
        assert_eq!(lane_geometry(c), (1, 4), "zcrx physical minima hold");
    }
    // The floors the refusal arithmetic reads are the pinned shipped ones.
    assert_eq!(Q_DEPTH_FLOOR, 4, "the never-regress depth floor");
    assert_eq!(PAYLOAD_BASE, 1024 * 1024, "the shipped payload ent");
}

/// share=1 (the default) is byte-identical to today's whole-machine
/// posture — solo-dark: no divided value, no new refusal, at any budget.
#[test]
fn fleet_share_one_is_byte_identical_to_the_whole_machine_posture() {
    use mem_budget::{fleet_share_floor_refusal, resolve_budget_from, resolve_budget_from_shared};
    for ram in [4 * GIB, 251_000_000_000, FLEET_RAM] {
        assert_eq!(
            resolve_budget_from_shared(None, None, None, ram, 1),
            resolve_budget_from(None, None, None, ram)
        );
        assert_eq!(
            resolve_budget_from_shared(None, None, Some(10 * GIB), ram, 1),
            resolve_budget_from(None, None, Some(10 * GIB), ram)
        );
        assert_eq!(
            resolve_budget_from_shared(Some(2 * GIB), None, None, ram, 1),
            resolve_budget_from(Some(2 * GIB), None, None, ram)
        );
    }
    // Solo never refuses — even a zero budget is today's (non-refusing)
    // never-regress posture, not a fleet-share arithmetic failure.
    assert_eq!(fleet_share_floor_refusal(1, 0, 4096), None);
}

/// A divided root that makes a floor unsatisfiable REFUSES the mount
/// loud, naming the arithmetic (never a silent clamp below a floor).
/// The floor demand is the kernel-mandated transport geometry's
/// never-divided minimum: possible_cpus × Q_DEPTH_FLOOR × PAYLOAD_BASE —
/// the queue COUNT cannot divide (the exemption class), depth floors at
/// 4, so this footprint exists per daemon regardless of share.
#[test]
fn fleet_share_unsatisfiable_floor_refuses_naming_the_arithmetic() {
    use mem_budget::fleet_share_floor_refusal;
    // 32 possible CPUs ⇒ floor = 32 × 4 × 1 MiB = 128 MiB.
    let floor = 32u64 * 4 * MIB;
    assert_eq!(floor, 134_217_728);
    // A satisfied floor mounts (boundary inclusive).
    assert_eq!(fleet_share_floor_refusal(2, floor, 32), None);
    assert_eq!(fleet_share_floor_refusal(2, floor + 1, 32), None);
    // Below the floor: refusal, naming every number in the arithmetic.
    let msg = fleet_share_floor_refusal(1024, floor - 1, 32)
        .expect("an unsatisfiable floor must refuse the mount");
    assert!(
        msg.contains("SQUEEZEFS_FLEET_SHARE=1024"),
        "the refusal names the share: {msg}"
    );
    assert!(
        msg.contains("134217727"),
        "the refusal names the divided budget: {msg}"
    );
    assert!(
        msg.contains("134217728"),
        "the refusal names the floor demand: {msg}"
    );
    assert!(
        msg.contains("Q_DEPTH_FLOOR"),
        "the refusal names the floor law: {msg}"
    );
    assert!(
        msg.contains("32"),
        "the refusal names the possible-CPU count"
    );
    // Solo (share 1) never refuses — pinned above too; both directions.
    assert_eq!(fleet_share_floor_refusal(1, floor - 1, 32), None);
}

/// The kernel-mandated-geometry exemption LIST is pinned to exactly the
/// kernel-mandated set (§5.6): the FUSE-over-io_uring queue COUNT (one
/// queue per kernel possible CPU or the session never becomes ready —
/// `fuse_over_uring.rs:2399/:2425`) and its structural shadows. The pin
/// is the `possible_cpus(` consumer census across the production trees:
/// a future kernel-mandated input joins the exemption EXPLICITLY (by
/// extending this list with its classification), never by drift — and a
/// derived cap reaching for the exempt root instead of the divided one
/// is a red test, not a judgment call.
#[test]
fn fleet_share_exemption_list_is_pinned_to_the_kernel_mandated_set() {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                rust_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    for tree in [
        "src",
        "crates/fuse3/src",
        "crates/squeezefs-ipc/src",
        "crates/squeezefs-preload/src",
    ] {
        rust_files(&root.join(tree), &mut files);
    }
    assert!(
        files.len() > 100,
        "census roots wrong: {} files",
        files.len()
    );
    let mut consumers = BTreeSet::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        if text.contains("possible_cpus(") {
            consumers.insert(
                f.strip_prefix(root)
                    .expect("census file outside the manifest root")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    let expected: BTreeSet<String> = [
        // The exempt root's definition (and the RAW-mask fallback that
        // keeps kernel geometry undivided even when sysconf fails).
        "src/cpu.rs",
        // The op registry shadows the DELIVERED ring geometry
        // (possible_cpus × Q_DEPTH_DESIRED) — a structural shadow of the
        // exempt queue count, never a sizing choice.
        "src/fuse_client.rs",
        // The fleet-share floor refusal PRICES the exempt geometry's
        // never-divided footprint (possible_cpus × Q_DEPTH_FLOOR ×
        // payload) against the divided budget — it reads the exempt
        // root because the demand it names cannot scale with the share.
        "src/mem_budget.rs",
        // D-3: the DLM-class stripe widths shadow the DELIVERED ring
        // geometry (possible_cpus × q_depth = the most ops the transport
        // can hold in flight against one daemon's lock tables) — the op
        // registry's class; N co-located daemons each face their own
        // full ring, so the width is never divided.
        "src/stripe_locks.rs",
        // The kernel-mandated queue COUNT itself (fuse_uring_create():
        // one queue per possible CPU; fewer never becomes ready) and the
        // qid-is-cpu identity check.
        "crates/fuse3/src/raw/connection/fuse_over_uring.rs",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        consumers, expected,
        "the possible_cpus consumer census drifted — a new consumer must \
         join the §5.6 exemption list EXPLICITLY (kernel-mandated) or read \
         the divided sizing root (crate::cpu::process_parallelism) instead"
    );
    // The SECOND exemption class (PR 13c, F-B3): the FLEET-WIDTH root —
    // a listener's load is the whole fleet's width, so N co-located
    // daemons each serve it whole and the RAW mask is the root. Its
    // consumer census is pinned the same way.
    let mut raw_consumers = BTreeSet::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        if text.contains("raw_parallelism(") {
            raw_consumers.insert(
                f.strip_prefix(root)
                    .expect("census file outside the manifest root")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    let raw_expected: BTreeSet<String> = [
        // The root's definition (and possible_cpus' RAW fallback).
        "src/cpu.rs",
        // The cluster-wire listener cap: a LISTENER serves the fleet's
        // width whatever this box's share (F-B3).
        "src/cluster_wire.rs",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        raw_consumers, raw_expected,
        "the raw_parallelism consumer census drifted — a new consumer must \
         join the fleet-width exemption class EXPLICITLY (a listener's load \
         is the fleet's, never this daemon's share) or read the divided root"
    );
}

/// ENG-10: the knob is registered (int, lo = 1 — zero and negatives
/// refuse at startup per the registry law), default 1, and malformed
/// values refuse rather than silently defaulting.
#[test]
fn fleet_share_knob_is_registered_and_zero_refuses_at_startup() {
    use squeezefs::env_knobs::{self, Kind};
    let knob = env_knobs::lookup("SQUEEZEFS_FLEET_SHARE")
        .expect("SQUEEZEFS_FLEET_SHARE must be registered");
    match knob.kind {
        Kind::Int { lo, hi } => {
            assert_eq!(lo, 1, "0 must be OUT of the admissible range");
            assert!(hi >= 4096, "room for any plausible fleet width");
        }
        other => panic!("SQUEEZEFS_FLEET_SHARE must be an Int knob, got {other:?}"),
    }
    assert_eq!(knob.default, "1", "default = today's whole-machine posture");
    for bad in ["0", "-1", "junk", "1.5"] {
        let v = env_knobs::validate_vars([("SQUEEZEFS_FLEET_SHARE", bad)]);
        assert!(
            !v.errors.is_empty(),
            "SQUEEZEFS_FLEET_SHARE={bad} must refuse at startup"
        );
    }
    for good in ["1", "4", "32"] {
        let v = env_knobs::validate_vars([("SQUEEZEFS_FLEET_SHARE", good)]);
        assert!(v.errors.is_empty(), "SQUEEZEFS_FLEET_SHARE={good} is valid");
    }
}

// ---------------------------------------------------------------------------
// KD-MW-14 rung 3c — the fleet-share RESIDUE list (the 3b follow-on):
// every pre-existing DAEMON-SIZING site that bypassed the divided root via
// a direct `std::thread::available_parallelism()` read now derives through
// a pure tie-tested form fed by `crate::cpu::process_parallelism()` (which
// also retires each site's Hang-1 pinned-first-toucher exposure by
// construction — the process mask is calling-thread independent). share=4
// rows prove the quarter; floors are never divided; the reader census pins
// the surviving non-daemon set so a 15th direct reader is a red test.
// ---------------------------------------------------------------------------

/// share=4 quarters every residue site's derivation (via the pure forms —
/// division happens at the CPU root, so each formula applied to the
/// shared root must produce exactly the quartered value wherever it is
/// above its floor/clamps; where a clamp masks the quarter at the field
/// shape, a bigger canonical box shows it and the field row pins the
/// clamp).
#[test]
fn fleet_share_quarters_the_residue_site_derivations() {
    use squeezefs::cpu::effective_parallelism_from;

    // The field box (32 CPUs) at share=4 ⇒ the 8-CPU effective root.
    let field = effective_parallelism_from(32, 4);
    assert_eq!(field, 8);

    // squeezefs-ipc blocking pool cap (cpus × 16, floor 8) — the daemon
    // FEEDS the divided root at startup (`set_sizing_parallelism`); the
    // pure form is what it sizes with.
    use squeezefs_ipc::sqz_blocking::pool_cap_from;
    assert_eq!(pool_cap_from(32), 512);
    assert_eq!(pool_cap_from(field), 128, "= 512/4");

    // Whole-block buffer pools (cores × 16, floor 64).
    use squeezefs::cache::pool::{block_pool_capacity_from, ranged_pool_capacity_from};
    assert_eq!(block_pool_capacity_from(32), 512);
    assert_eq!(block_pool_capacity_from(field), 128, "= 512/4");
    // Ranged read-bounce pool (cores × 64, floor 512): the quarter lands
    // exactly ON the floor at the field shape.
    assert_eq!(ranged_pool_capacity_from(32), 2048);
    assert_eq!(ranged_pool_capacity_from(field), 512, "= 2048/4");

    // LRU shard count (next_power_of_two, floor 16): the quarter is
    // visible on the 128-core box; the field box lands on the floor.
    use squeezefs::cache::lru::shard_count_from;
    assert_eq!(shard_count_from(128), 128);
    assert_eq!(shard_count_from(effective_parallelism_from(128, 4)), 32);
    assert_eq!(shard_count_from(field), 16, "shard floor holds");

    // Crypto scratch-pool capacity (cores/4, clamp [4, 16]): the quarter
    // is visible on the 64-core box; the field share lands on the floor.
    use squeezefs::crypto_compress::scratch_pool_capacity_from;
    assert_eq!(scratch_pool_capacity_from(64), 16);
    assert_eq!(
        scratch_pool_capacity_from(effective_parallelism_from(64, 4)),
        4,
        "= 16/4"
    );

    // cluster_wire owner-side RPC lanes (ceil(cpus/8), clamp [1, 8]).
    use squeezefs::cluster_wire::service_threads_from;
    assert_eq!(service_threads_from(None, 32), 4);
    assert_eq!(service_threads_from(None, field), 1, "= 4/4");
    // The cluster_wire connection cap is NOT a residue site any more (PR
    // 13c, F-B3): a listener's load is the FLEET's width, which the share
    // divisor shrinks it by — it reads the RAW root (the fleet-width
    // exemption class); its rows are `listener_cap_derives_from_the_raw_root…`.

    // meta_ship per-frame batch cap (cpus × 2, clamp [64, 4096]): the
    // quarter is visible on the 256-core box; the field shape sits on the
    // M7 floor at both shares.
    use squeezefs::meta_ship::router::batch_max_from;
    assert_eq!(batch_max_from(None, 256), 512);
    assert_eq!(
        batch_max_from(None, effective_parallelism_from(256, 4)),
        128,
        "= 512/4"
    );
    assert_eq!(batch_max_from(None, field), 64, "M7 floor holds");

    // meta_ship publish-plane in-flight frame depth (ceil(cpus/8), clamp
    // [2, 8] — the owner RPC-lane slope, D-1b): the quarter is visible on
    // the 32-core box; the field shape sits on the double-buffer floor.
    use squeezefs::meta_ship::publish::publish_ship_depth_from;
    assert_eq!(publish_ship_depth_from(None, 32), 4);
    assert_eq!(publish_ship_depth_from(None, 256), 8, "RPC-lane ceiling");
    assert_eq!(publish_ship_depth_from(None, field), 2, "depth floor holds");

    // Job-fabric workers (cpus/4, clamp [2, 8]).
    use squeezefs::jobs::fabric_workers_default;
    assert_eq!(fabric_workers_default(32), 8);
    assert_eq!(fabric_workers_default(field), 2, "= 8/4");

    // Format-verb concurrent volume-format pool (cpus × 2).
    use squeezefs::config_ops::format_pool_permits;
    assert_eq!(format_pool_permits(32), 64);
    assert_eq!(format_pool_permits(field), 16, "= 64/4");

    // sqz-meta lane population (2, or 1 on an EFFECTIVE uniprocessor —
    // a share can collapse a 2-CPU box to one lane).
    use squeezefs::meta_exec::meta_lanes_from;
    assert_eq!(meta_lanes_from(32), 2);
    assert_eq!(meta_lanes_from(field), 2);
    assert_eq!(meta_lanes_from(effective_parallelism_from(2, 4)), 1);
}

/// Floors hold undivided at every share (the 2-CPU floor box collapsed
/// to an effective 1): silent starvation below a floor is never what a
/// share produces — physical minima / never-regress-below-shipped only.
#[test]
fn residue_site_floors_hold_at_every_share() {
    use squeezefs::cpu::effective_parallelism_from;
    for share in [2usize, 4, 32, 4096] {
        let c = effective_parallelism_from(2, share);
        assert_eq!(c, 1, "the CPU root floors at 1 (share={share})");
        assert!(
            squeezefs_ipc::sqz_blocking::pool_cap_from(c) >= 8,
            "blocking-pool floor holds at share={share}"
        );
        assert_eq!(
            squeezefs::cache::pool::block_pool_capacity_from(c),
            64,
            "whole-block pool floor holds at share={share}"
        );
        assert_eq!(
            squeezefs::cache::pool::ranged_pool_capacity_from(c),
            512,
            "ranged pool floor holds at share={share}"
        );
        assert_eq!(
            squeezefs::cache::lru::shard_count_from(c),
            16,
            "LRU shard floor holds at share={share}"
        );
        assert_eq!(
            squeezefs::crypto_compress::scratch_pool_capacity_from(c),
            4,
            "scratch-pool floor holds at share={share}"
        );
        assert_eq!(
            squeezefs::cluster_wire::service_threads_from(None, c),
            1,
            "RPC-lane floor holds at share={share}"
        );
        assert_eq!(
            squeezefs::meta_ship::router::batch_max_from(None, c),
            64,
            "M7 batch floor holds at share={share}"
        );
        assert_eq!(
            squeezefs::meta_ship::publish::publish_ship_depth_from(None, c),
            2,
            "publish-depth floor holds at share={share}"
        );
        assert_eq!(
            squeezefs::jobs::fabric_workers_default(c),
            2,
            "fabric-worker floor holds at share={share}"
        );
        assert_eq!(
            squeezefs::config_ops::format_pool_permits(c),
            2,
            "format pool never sizes to 0 at share={share}"
        );
        assert_eq!(
            squeezefs::meta_exec::meta_lanes_from(c),
            1,
            "uniprocessor lane minimum at share={share}"
        );
    }
}

/// Explicit levers stay verbatim through the extracted pure forms (the
/// ipc-cap precedence law: explicit wins, bounded only by its own
/// admissible range / the never-oversubscribe rail — never re-divided).
#[test]
fn residue_site_explicit_levers_stay_verbatim() {
    use squeezefs::cluster_wire::service_threads_from;
    use squeezefs::meta_ship::router::batch_max_from;
    assert_eq!(batch_max_from(Some(8), 1), 8, "explicit wins verbatim");
    assert_eq!(batch_max_from(Some(9999), 256), 4096, "range clamp only");
    use squeezefs::meta_ship::publish::publish_ship_depth_from;
    assert_eq!(
        publish_ship_depth_from(Some(1), 256),
        1,
        "the stop-and-wait A/B control wins verbatim"
    );
    assert_eq!(
        publish_ship_depth_from(Some(999), 1),
        64,
        "range clamp only"
    );
    assert_eq!(service_threads_from(Some(6), 32), 6, "explicit wins");
    assert_eq!(
        service_threads_from(Some(16), 4),
        4,
        "never oversubscribes the (divided) root"
    );
}

// ---------------------------------------------------------------------------
// Symmetric PR 13c — F-B3: the cluster-wire LISTENER cap derives from the
// width it serves (`.benchmarks/2026-09-19-sym-acceptance.md` §3.9.2). The
// PR-13b form `(cpus × 16).clamp(64, 1024)` read the FLEET-SHARE-DIVIDED
// root: on 32 cores under `SQUEEZEFS_FLEET_SHARE=32` it derived 64 — the
// listener's cap SHRANK exactly as the fleet it served grew, and the 14th
// member of a 32-member fleet was refused at accept. A listener's load is
// the fleet's width × each member's session demand, independent of how
// many daemons share this box's CPUs: the RAW mask is its root (the
// fleet-width exemption class, §5.6), the fd budget its ceiling.
// ---------------------------------------------------------------------------

/// The cap's pure form: `(raw_cpus × 16)` — 16 parked connection threads
/// per core, the thread-per-connection posture — floored at the shipped 64
/// and ceilinged by the fd budget: each connection holds TWO fds (the
/// socket + its shutdown-nudge clone) and the listeners may hold a quarter
/// of `RLIMIT_NOFILE` (the `uring_fs::fd_cache_cap` law — the FUSE rings,
/// device fds, staging segments and the job wire own the rest), so
/// `ceiling = nofile / 8`, never below the floor.
#[test]
fn listener_cap_derives_from_the_raw_root_and_the_fd_budget() {
    use squeezefs::cluster_wire::{max_connections_from, CONNECTION_FDS, LISTENER_FD_SHARE};
    assert_eq!(CONNECTION_FDS, 2, "the socket + its shutdown-nudge clone");
    assert_eq!(
        LISTENER_FD_SHARE, 4,
        "a quarter of RLIMIT_NOFILE — fd_cache_cap's law"
    );
    // The box: 32 raw CPUs, a raised soft limit.
    assert_eq!(max_connections_from(32, 524_288), 512);
    // The fd budget ceilings a big box on a small limit: 1024 / 2 / 4 = 128.
    assert_eq!(max_connections_from(256, 1_024), 128);
    assert_eq!(
        max_connections_from(256, 65_536),
        4096,
        "256 × 16 under the budget"
    );
    // The floor is the shipped posture even under a tiny fd limit (a
    // process that small fails at its FUSE rings first).
    assert_eq!(max_connections_from(1, 1_024), 64);
    assert_eq!(max_connections_from(1, 64), 64);
}

/// THE F-B3 LAW: the cap is fleet-share EXEMPT. On the box's shape (32 raw
/// CPUs, 32 co-located members, each under `FLEET_SHARE=32`) the listener
/// admits the whole fleet's steady-state session demand — computed from the
/// SAME pool derivations the members dial with — where the PR-13b form
/// admitted 64 of the 160+ sessions.
/// The dial sites that hold a STANDING session against the manager's S8
/// listener, counted off their marker (`S8-LISTENER CONTROL SESSION
/// (member_session_demand_from's census)`) — the code-side term the
/// per-member demand constant is tied to. A new long-lived dial site
/// carries the marker or the demand law reads an undercount.
fn s8_listener_control_session_sites() -> usize {
    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                rust_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    assert!(
        files.len() > 100,
        "census root wrong: {} files",
        files.len()
    );
    let marker = "// S8-LISTENER CONTROL SESSION (member_session_demand_from's census)";
    files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .map(|text| text.matches(marker).count())
        .sum()
}

#[test]
fn a_32_member_fleets_session_demand_fits_the_listener_cap_at_share_32() {
    use squeezefs::cluster_wire::{max_connections_from, member_session_demand_from};
    use squeezefs::cpu::effective_parallelism_from;
    let raw = 32usize;
    let members = 32usize;
    let volumes = 2usize; // the fleet rig's MDS_COUNT
    let member_cpus = effective_parallelism_from(raw, members);
    assert_eq!(member_cpus, 1, "each co-located member sizes for one CPU");
    let per_member = member_session_demand_from(member_cpus, volumes);
    // A member's demand against the S8 listener: per volume the token
    // grant pool (publish_ship_depth = 2 at one CPU) + its recall channel,
    // plus the publish pool and the control sessions a joined writer
    // holds — COUNTED IN CODE below, never a number asserted against
    // itself (review round 1, Issue 3).
    assert_eq!(
        per_member,
        2 * (2 + 1) + 2 + squeezefs::cluster_wire::MEMBER_CONTROL_SESSIONS
    );
    assert_eq!(
        squeezefs::cluster_wire::MEMBER_CONTROL_SESSIONS,
        s8_listener_control_session_sites(),
        "MEMBER_CONTROL_SESSIONS ≡ the marked long-lived dial sites in src/"
    );
    let cap = max_connections_from(raw, 524_288);
    assert!(
        members * per_member <= cap,
        "32 members × {per_member} sessions = {} must fit the cap {cap}",
        members * per_member
    );
    // The PR-13b defect, stated as arithmetic: the divided root's cap.
    let divided = (member_cpus * 16).clamp(64, 1024);
    assert!(
        members * per_member > divided,
        "the fleet-share-divided cap {divided} refused the fleet ({})",
        members * per_member
    );
}

/// The explicit lever wins verbatim inside its range and is RAILED by the
/// fd budget (the `service_threads_from(Some(16), 4) == 4` precedent: an
/// explicit value the process cannot hold is a lie, never a cap).
#[test]
fn listener_cap_explicit_lever_is_railed_by_the_fd_budget() {
    use squeezefs::cluster_wire::max_connections_resolved;
    assert_eq!(max_connections_resolved(Some(200), 32, 524_288), 200);
    assert_eq!(
        max_connections_resolved(Some(4096), 32, 1_024),
        128,
        "the fd budget rails an explicit value"
    );
    assert_eq!(max_connections_resolved(None, 32, 524_288), 512);
}

/// D-1c (e2e perf audit §5.3 row 1 — one conveyor group per shipped
/// frame): **a frame fits one drain.** The publish plane's per-frame call
/// cap and the M7 conveyor's per-drain tx cap derive from the SAME root
/// with the same shape (`cpus × 2`, floor 64), so a full frame's group —
/// enqueued under one queue-lock acquisition — is never split by the tx
/// cap; the only cap that may split a group is the byte cap (the
/// progress law). Drift between the two derivations would silently turn
/// "one frame = one pass" back into a venue ratio.
#[test]
fn a_shipped_frame_fits_one_conveyor_drain_at_every_width() {
    use squeezefs::meta_backend::kv::backend::resolve_commit_batch_txs;
    use squeezefs::meta_ship::router::batch_max_from;
    for cpus in [1usize, 2, 4, 8, 16, 32, 64, 128, 192] {
        assert!(
            batch_max_from(None, cpus) <= resolve_commit_batch_txs(None, cpus),
            "cpus={cpus}: frame cap {} exceeds the conveyor tx cap {} — a full frame would \
             span two drains",
            batch_max_from(None, cpus),
            resolve_commit_batch_txs(None, cpus)
        );
    }
}

/// The `std::thread::available_parallelism` reader census: after rung 3c
/// the direct readers are exactly the NON-daemon-sizing set — the sizing
/// roots' own syscall fallbacks (src/cpu.rs, sqz_blocking's unfed-embedding
/// fallback), the bench tool's load generation, the CLIENT-process shim,
/// and the fuse3 fork's internal fallbacks/shards (its own excluded
/// workspace; the transport's queue COUNT is the exempt possible-CPUs
/// census above). A new direct reader
/// is a red test, never a drift: daemon sizing reads the divided root
/// (`crate::cpu::process_parallelism`) — or, inside squeezefs-ipc, the
/// parallelism the daemon feeds — instead.
#[test]
fn available_parallelism_reader_census_is_pinned_to_the_non_daemon_set() {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                rust_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    for tree in [
        "src",
        "crates/fuse3/src",
        "crates/squeezefs-ipc/src",
        "crates/squeezefs-preload/src",
    ] {
        rust_files(&root.join(tree), &mut files);
    }
    assert!(
        files.len() > 100,
        "census roots wrong: {} files",
        files.len()
    );
    let mut readers = BTreeSet::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            continue;
        };
        if text.contains("std::thread::available_parallelism") {
            readers.insert(
                f.strip_prefix(root)
                    .expect("census file outside the manifest root")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    let expected: BTreeSet<String> = [
        // The divided sizing root's OWN syscall fallback (and module doc).
        "src/cpu.rs",
        // The bench TOOL's load-generation width — measures the machine
        // under test, not a daemon resource (class c).
        "src/bench.rs",
        // The blocking pool's unfed-embedding fallback: the daemon feeds
        // the divided root via `set_sizing_parallelism` at startup;
        // foreign hosts/unit tests fall back to the raw mask.
        "crates/squeezefs-ipc/src/sqz_blocking.rs",
        // The CLIENT-process shim sizes the client's sessions from the
        // client's own mask — the fleet divisor governs daemon resources
        // only (class c).
        "crates/squeezefs-preload/src/interpose.rs",
        // fuse3 fork (its own excluded workspace): the TPC scheduler's
        // defensive fallback for an empty process mask, the phase-table
        // shard count, and the kernel-possible-CPUs sysconf fallback
        // (the exempt geometry class).
        "crates/fuse3/src/raw/session.rs",
        "crates/fuse3/src/raw/read_phase.rs",
        "crates/fuse3/src/raw/connection/fuse_over_uring.rs",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        readers, expected,
        "the available_parallelism reader census drifted — a daemon-sizing \
         site must read the divided root (crate::cpu::process_parallelism, \
         or the parallelism the daemon feeds into squeezefs-ipc), and a \
         genuine non-daemon reader joins this list EXPLICITLY with its \
         classification"
    );
}

// ---------------------------------------------------------------------------
// Hybrid lane gate (2026-08-07): the shim's kernel-lane crossover threshold
// ---------------------------------------------------------------------------

/// Ruling D14's fleet-posture corollary ("the shim narrows toward the
/// IOPS lane") formalized as a DERIVED crossover, never a constant: the
/// threshold is the op size where the ring's per-byte cost (client copy
/// passes over the payload at the probed memcpy bandwidth — 2 for reads:
/// daemon serve-into-arena + client consume; the write side's single
/// copy crosses at 2× and collapses to the same clamp on measured
/// hardware) exceeds the kernel FUSE lane's per-op overhead delta
/// ([`KERNEL_LANE_RTT_DELTA_NS`] — the D14 field bracket's measured
/// rand-4k per-op cost gap class). Rails are PHYSICAL: floor = the slot
/// slab (at or below it a ring op is the single-flight allocation-free
/// serial path — the IOPS lane's own regime, which the gate exists to
/// protect, never to tax); ceiling = `max_op_bytes` (past it an op must
/// chunk into multiple ring flights, so the gate must have engaged at
/// latest there). `SQUEEZEFS_IL_KERNEL_LANE_MIN` overrides verbatim
/// (0 = gate off — the A/B lever); the derived value is exported live as
/// `ipc_lane_gate_threshold_bytes`.
#[test]
fn il_kernel_lane_min_derives_from_membw_and_lane_rtt_delta() {
    use squeezefs_ipc::sizing::{kernel_lane_min_default, KERNEL_LANE_RTT_DELTA_NS};
    // Default session geometry: 64 MiB arena / 1024 slots ⇒ 64 KiB slab;
    // 1 MiB per-op ceiling.
    const SLAB: u64 = 64 * 1024;
    const MAX_OP: u64 = 1024 * 1024;
    // The delta class constant is measured evidence, pinned so a retune
    // is a deliberate act with a new citation.
    assert_eq!(KERNEL_LANE_RTT_DELTA_NS, 2_300);
    // The field-class client (~15 GB/s single-thread memcpy): raw
    // crossover ≈ 17 KiB sits BELOW the slab ⇒ clamps to the slab floor —
    // single-flight ops stay ring, everything that would chunk multi-slab
    // rides the kernel zc lane.
    assert_eq!(kernel_lane_min_default(15_000_000_000, SLAB, MAX_OP), SLAB);
    // A failed/degenerate probe (0) degrades to the same floor.
    assert_eq!(kernel_lane_min_default(0, SLAB, MAX_OP), SLAB);
    // A 100 GB/s-class copy engine: interior value on the 4 KiB LBA/page
    // grain (100e9 × 2300 ns ÷ 1e9 ÷ 2 copies = 115_000 → 28 × 4096).
    assert_eq!(
        kernel_lane_min_default(100_000_000_000, SLAB, MAX_OP),
        112 * 1024
    );
    // Absurd bandwidth rails at the payload ceiling, overflow-safe.
    assert_eq!(kernel_lane_min_default(u64::MAX, SLAB, MAX_OP), MAX_OP);
    // Toy geometries: the rails never invert (lo ≤ hi holds even when a
    // caller hands slab == max_op).
    assert_eq!(kernel_lane_min_default(0, 4096, 4096), 4096);
}

/// §5.6 / KD-MW-14 (rung-7 S6-a finding, 2026-08-16): the cgroup
/// UNRECLAIMABLE arm is a SHARED system root on a co-located fleet — N
/// daemons read ONE `memory.stat`, so feeding the whole cage's residue
/// into a level machine whose budget was divided by N over-fires every
/// daemon's Red/backstop ~N× on a healthy quiet fleet (the live S6-a
/// N=32 row: every daemon budget 2.76 GB, shared unreclaimable sample
/// 4.8 GB, ALL 32 in Red with hard backstops while their own ledgers
/// read 0.5 GB). The sample must divide at the root exactly like the
/// budget's system roots do — one divisor, downstream formulas
/// untouched; share 1 is the identity. Each daemon's OWN balloon stays
/// policed undivided by the per-process RSS arm.
#[test]
fn the_unreclaimable_arm_divides_by_the_fleet_share_like_every_system_root() {
    use squeezefs::mem_budget::tick_inputs;
    const GIB: u64 = 1024 * 1024 * 1024;
    // Share 1: byte-identical passthrough (the solo posture).
    assert_eq!(
        tick_inputs(2 * GIB, GIB, 5 * GIB, 1),
        (2 * GIB, GIB, 5 * GIB)
    );
    // Share 32: the shared-cgroup sample enters divided (ceil), so the
    // fleet's collective residue compares against the collective budget
    // — not the whole cage against one daemon's 1/32nd slice.
    let (budget, rss, unreclaimable) = tick_inputs(2 * GIB, GIB, 5 * GIB, 32);
    assert_eq!(
        budget,
        2 * GIB,
        "the budget was already shared at its roots"
    );
    assert_eq!(
        rss, GIB,
        "RSS is per-process by construction — never divided"
    );
    assert_eq!(
        unreclaimable,
        5 * GIB / 32,
        "the cgroup unreclaimable sample divides at the root, the \
         fleet_shared_root law (5 GiB divides 32 evenly)"
    );
    // The rounding face: ceil, never floor (the fleet_shared_root law's
    // own contract — a divided root never under-states).
    assert_eq!(
        tick_inputs(2 * GIB, GIB, 5 * GIB + 1, 32).2,
        5 * GIB / 32 + 1,
        "a non-even sample rounds UP"
    );
}

// ---------------------------------------------------------------------------
// W-4 — block-reclaim queue cap + at-cap park bound (e2e perf audit ladder
// row 14; write-wall OQ-5 discharged): the two liveness constants become
// derivations of the reclaimer's OWN measured drain and the write side's
// displacement rate. Evidence note:
// `.benchmarks/2026-09-05-w4-reclaim-derivation.md`.
// ---------------------------------------------------------------------------

/// The drain's ROOM LATENCY — the time the reclaimer needs to free one
/// batch of queue slots at its measured aggregate drain rate
/// (`batch_blocks ÷ drain_rate`). This is the "drain service time for
/// one batch" both derivations key on: width-blind by construction (under
/// target-bound load the aggregate rate is what the target delivers at
/// ANY client width — write-wall E2), so it never inflates with the lane
/// count the way a per-lane wall does. Cold (no drain measured) = the
/// shipped 1 s park bound — the assumption the shipped constant encoded.
#[test]
fn reclaim_room_latency_is_batch_over_measured_drain_rate() {
    use squeezefs::block_reclaim::{room_latency_ms, PARK_BOUND_CEILING_MS};
    // Fleet under load: ~1,800 blocks/s (write-wall §6.2 E2, any width),
    // the shipped 64-block batch ⇒ 35 ms per batch of room.
    assert_eq!(room_latency_ms(64, 1_800), 35);
    // Idle catch-up at width 32: ~5,900 cmd/s ⇒ 10 ms.
    assert_eq!(room_latency_ms(64, 5_900), 10);
    // A slow substrate at 100 blocks/s ⇒ 640 ms.
    assert_eq!(room_latency_ms(64, 100), 640);
    // Never 0 while measured (a sub-ms batch still costs a tick of room).
    assert_eq!(room_latency_ms(64, 1_000_000), 1);
    // Cold = the shipped assumption.
    assert_eq!(room_latency_ms(64, 0), PARK_BOUND_CEILING_MS);
}

/// `SQUEEZEFS_RECLAIM_CAP_PARK_MS` default: `clamp(4 × room_latency,
/// 50 ms, 1000 ms)`. A parked producer that saw no room edge in four
/// batches' worth of drain time is waiting on a STALLED drain, not a slow
/// one (room is made per coalesced range, so a live drain at any rate
/// makes room within one batch-time) — overflow is then the right answer.
/// Floor 50 ms = the worker's manners tick (its coarsest scheduling
/// quantum: the at-cap wake short-circuits it, but a bound below the
/// guaranteed cadence could trip on a healthy worker whose wake sat behind
/// a saturated blocking pool). Ceiling 1000 ms = the shipped constant —
/// the derived bound never parks a producer LONGER than the shipped
/// posture did (never-regress applied to a tail bound).
#[test]
fn reclaim_park_bound_derives_from_the_drains_room_latency() {
    use squeezefs::block_reclaim::{
        derived_park_bound_ms, room_latency_ms, PARK_BOUND_BATCHES, PARK_BOUND_CEILING_MS,
        PARK_BOUND_FLOOR_MS,
    };
    assert_eq!(PARK_BOUND_BATCHES, 4);
    assert_eq!(PARK_BOUND_FLOOR_MS, 50);
    assert_eq!(PARK_BOUND_CEILING_MS, 1000);
    // Fleet under load: 4 × 35 = 140 ms — 7× below the shipped 1 s tail.
    assert_eq!(derived_park_bound_ms(room_latency_ms(64, 1_800)), 140);
    // Idle/fast drains floor at the manners tick.
    assert_eq!(derived_park_bound_ms(room_latency_ms(64, 5_900)), 50);
    assert_eq!(derived_park_bound_ms(room_latency_ms(64, 100_000)), 50);
    // A slow substrate never parks longer than shipped.
    assert_eq!(derived_park_bound_ms(room_latency_ms(64, 100)), 1000);
    // Cold = shipped.
    assert_eq!(derived_park_bound_ms(room_latency_ms(64, 0)), 1000);
    // Monotone in room latency between the clamps.
    assert!(derived_park_bound_ms(20) < derived_park_bound_ms(30));
}

/// `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS` default: `clamp(displacement_rate
/// × room_latency, 4096, ram_ceiling)` — the blocks the write side
/// displaces while the drain makes one batch of room, i.e. the buffer a
/// keeping-pace drain needs so producers never park. Floor 4096 = the
/// shipped posture (never regress below). The ceiling is RAM: entries
/// are bookkeeping (no payload — the deferred bytes live on the device
/// and are gauged as `queue_bytes`), so the queue may hold 1/1024 of the
/// R5 budget, capped at the registry's 2^20 admissible maximum.
#[test]
fn reclaim_queue_cap_derives_from_displacement_rate_and_room_latency() {
    use squeezefs::block_reclaim::{
        derived_queue_cap_blocks, queue_cap_ceiling_blocks, reclaim_entry_ram_bytes,
        room_latency_ms, QUEUE_CAP_FLOOR_BLOCKS, QUEUE_RAM_SHARE_DIVISOR,
    };
    assert_eq!(QUEUE_CAP_FLOOR_BLOCKS, 4096);
    assert_eq!(QUEUE_RAM_SHARE_DIVISOR, 1024);
    let room_load = room_latency_ms(64, 1_800); // 35 ms
                                                // The fleet shape: 19 GB/s of 4 MiB rewrite displaces ~4,750
                                                // blocks/s; over 35 ms of room latency that is ~166 blocks — the
                                                // shipped floor governs (the cap was never the fleet's lever; the
                                                // park bound is — the drain cannot keep pace there at ANY cap).
    assert_eq!(
        derived_queue_cap_blocks(4_750, room_load, FIELD_BUDGET),
        QUEUE_CAP_FLOOR_BLOCKS
    );
    // The same bandwidth on 64 KiB blocks: ~300k blocks/s × 35 ms =
    // 10,500 — the derivation lifts the cap above the floor (it scales
    // with the rate input).
    assert_eq!(
        derived_queue_cap_blocks(300_000, room_load, FIELD_BUDGET),
        10_500
    );
    // Linear in the rate above the floor.
    assert_eq!(
        derived_queue_cap_blocks(600_000, room_load, FIELD_BUDGET),
        21_000
    );
    // Linear in the room latency too (a slower drain needs more buffer).
    assert_eq!(
        derived_queue_cap_blocks(300_000, 2 * room_load, FIELD_BUDGET),
        21_000
    );
    // Cold room latency (1 s) with a measured/seeded rate: one second of
    // displacement — the shipped bound's assumption made explicit.
    assert_eq!(
        derived_queue_cap_blocks(19_000, room_latency_ms(64, 0), FIELD_BUDGET),
        19_000
    );
    // Rate 0 (nothing displacing yet) ⇒ the floor.
    assert_eq!(
        derived_queue_cap_blocks(0, room_load, FIELD_BUDGET),
        QUEUE_CAP_FLOOR_BLOCKS
    );

    // The RAM ceiling: budget/1024 ÷ per-entry RAM, floored at the shipped
    // cap and capped at the registry maximum.
    let entry = reclaim_entry_ram_bytes();
    assert!(
        (64..=512).contains(&entry),
        "a reclaim entry is bookkeeping-sized ({entry} B): Arc + in-flight \
         guard + device path + offset/size + queue slot"
    );
    // Field: 176 GiB / 1024 = 176 MiB ÷ ~entry ⇒ past the 2^20 registry
    // maximum ⇒ 2^20.
    assert_eq!(queue_cap_ceiling_blocks(FIELD_BUDGET), 1 << 20);
    // Floor box: 2.8 GiB / 1024 = 2.8 MiB ÷ entry — a few thousand to a
    // few tens of thousands; equals the formula and never dips below the
    // shipped floor.
    let floor_ceiling = queue_cap_ceiling_blocks(FLOOR_BUDGET);
    assert_eq!(
        floor_ceiling,
        (FLOOR_BUDGET / QUEUE_RAM_SHARE_DIVISOR / entry).clamp(QUEUE_CAP_FLOOR_BLOCKS, 1 << 20)
    );
    assert!(floor_ceiling >= QUEUE_CAP_FLOOR_BLOCKS);
    assert!(floor_ceiling < 1 << 20);
    // The ceiling binds: a runaway rate on the floor box clamps to it.
    assert_eq!(
        derived_queue_cap_blocks(u64::MAX / 4, 1000, FLOOR_BUDGET),
        floor_ceiling
    );
    // A zero/unknown budget degrades to the floor (never 0, never below
    // shipped).
    assert_eq!(queue_cap_ceiling_blocks(0), QUEUE_CAP_FLOOR_BLOCKS);
    assert_eq!(
        derived_queue_cap_blocks(300_000, room_load, 0),
        QUEUE_CAP_FLOOR_BLOCKS
    );
}

/// Explicit knobs win verbatim over the derivation (the ipc-cap precedence
/// law); an out-of-range or malformed value falls through to the derived
/// default in-process (the startup gate refuses it before a mount).
#[test]
fn reclaim_knobs_explicit_wins_verbatim_over_derived() {
    use squeezefs::block_reclaim::{resolve_park_bound_ms, resolve_queue_cap_blocks};
    // Cap: the A0 lever (4096 restores the shipped constant exactly).
    assert_eq!(resolve_queue_cap_blocks(Some("4096"), 21_000), 4096);
    assert_eq!(resolve_queue_cap_blocks(Some("96"), 21_000), 96);
    assert_eq!(resolve_queue_cap_blocks(None, 21_000), 21_000);
    assert_eq!(
        resolve_queue_cap_blocks(Some("0"), 21_000),
        21_000,
        "below range"
    );
    assert_eq!(resolve_queue_cap_blocks(Some("junk"), 21_000), 21_000);
    // Park bound: `0` = never park (immediate soft overflow) stays a valid
    // explicit posture; 1000 restores the shipped constant exactly.
    assert_eq!(resolve_park_bound_ms(Some("1000"), 140), 1000);
    assert_eq!(resolve_park_bound_ms(Some("0"), 140), 0);
    assert_eq!(resolve_park_bound_ms(None, 140), 140);
    assert_eq!(
        resolve_park_bound_ms(Some("60001"), 140),
        140,
        "above range"
    );
    assert_eq!(resolve_park_bound_ms(Some("soon"), 140), 140);
}

/// The free-grace hold-time lever (d)'s rate limit derives from the plane's
/// own numbers — drift-is-red (`.benchmarks/2026-09-06-free-grace-hold-time.md`):
/// a bound recompute the dirty mark triggers is admitted no closer than
/// `ack_refresh_floor ÷ members` (the rate the min can change at) and
/// never closer than twice the scan's measured cost (a recompute may not
/// run more than half the time). The floor term governs small fleets, the
/// scan term the 15 k arithmetic; no free constant sits between them.
#[test]
fn free_grace_refresh_on_ack_interval_derives_from_floor_members_and_scan() {
    use squeezefs::free_grace::refresh_on_ack_interval_ms;
    // The s11 venue: 1 s floor, 8 members, a µs-class scan ⇒ 125 ms.
    assert_eq!(refresh_on_ack_interval_ms(1_000, 8, 0), 125);
    // A slower floor scales it; a single member reads the whole floor.
    assert_eq!(refresh_on_ack_interval_ms(5_000, 8, 0), 625);
    assert_eq!(refresh_on_ack_interval_ms(1_000, 1, 0), 1_000);
    assert_eq!(
        refresh_on_ack_interval_ms(1_000, 0, 0),
        1_000,
        "no members ⇒ the floor"
    );
    // 15 k members: floor ÷ members rounds to 0 and the scan term governs
    // — twice its measured cost, never a busy recompute.
    assert_eq!(refresh_on_ack_interval_ms(1_000, 15_000, 3), 6);
    // The scan term is a floor on the floor term, never a replacement.
    assert_eq!(refresh_on_ack_interval_ms(1_000, 8, 100), 200);
    assert_eq!(refresh_on_ack_interval_ms(1_000, 8, 50), 125);
}

/// The reader ack ladder's qualify window derives from the WRITER's
/// checkpoint landing ceiling (ladder re-derivation item 1,
/// `.benchmarks/2026-09-06-free-grace-ladder-rederivation.md`) — drift-is-red:
/// the ceiling is the cadence trigger plus two checkpoint-task tick periods
/// (the tick wait and the bounded maintenance drain the trigger is
/// evaluated behind), and the tick period is the ONE derivation the
/// checkpoint task and the reader's poll cadence both ride, so the three
/// cannot drift apart.
#[test]
fn free_grace_qualify_ceiling_derives_from_the_checkpoint_trigger_and_tick() {
    use squeezefs::meta_backend::kv::checkpoint::{
        checkpoint_landing_ceiling_ms, checkpoint_tick_period_ms, CHECKPOINT_MAX_AGE_MS,
    };
    use squeezefs::meta_backend::kv::revalidate::resolve_revalidate_interval_ms;
    for flush in [0u64, 1, 50, 250, 1_000, 5_000] {
        let tick = checkpoint_tick_period_ms(flush);
        assert_eq!(
            checkpoint_landing_ceiling_ms(flush),
            CHECKPOINT_MAX_AGE_MS as u64 + 2 * tick,
            "flush {flush}: trigger + 2 × tick"
        );
        // The reader's poll interval rides the same tick derivation.
        assert_eq!(
            resolve_revalidate_interval_ms(flush, None),
            tick.max(CHECKPOINT_MAX_AGE_MS as u64),
            "flush {flush}: the poll cadence is max(tick, trigger) off the same tick"
        );
    }
    // Strict mode reads the task's own 100 ms tick; the shipped 50 ms
    // flush lands at 1,100 — strictly below the 2,000 ms staleness bound
    // the pre-change window rode (the poll interval is not in it).
    assert_eq!(checkpoint_landing_ceiling_ms(0), 1_200);
    assert_eq!(checkpoint_landing_ceiling_ms(50), 1_100);
    assert!(checkpoint_landing_ceiling_ms(50) < resolve_revalidate_interval_ms(50, None) + 1_000);
    // The ladder's rule: lever on = ceiling + skew, off = staleness + skew.
    assert_eq!(
        squeezefs::free_grace::qualify_lag_ms(true, 1_100, 2_000, 22),
        1_122
    );
    assert_eq!(
        squeezefs::free_grace::qualify_lag_ms(false, 1_100, 2_000, 22),
        2_022
    );
}

/// The writer→member checkpoint composite's two derivations — drift-is-red
/// (`.benchmarks/2026-09-06-free-grace-checkpoint-composite.md`): the
/// elastic checkpoint ceiling is `max(P/2, 2 × measured cycle)` capped at
/// the writer's routine ceiling (half the reader's routine poll is the
/// Nyquist bound that puts a new root in every pass window; a cycle may
/// not run more than half the time — lever (d)'s law for the scan; never
/// slower than the routine), and the live prod floor is
/// `max(min(P, ceiling), skew)`, which at the routine ceiling IS the
/// shipped `max(P, skew)`. No free constant sits in either.
#[test]
fn free_grace_elastic_checkpoint_ceiling_derives_from_the_poll_and_the_cycle() {
    use squeezefs::free_grace::elastic_checkpoint_ceiling_ms;
    // The shipped venue: P = 1 s, a ≈ 20 ms cycle ⇒ 500 ms.
    assert_eq!(elastic_checkpoint_ceiling_ms(1_000, 20, 1_000), 500);
    // The cycle-cost floor governs a slow device; the routine caps it.
    assert_eq!(elastic_checkpoint_ceiling_ms(1_000, 300, 1_000), 600);
    assert_eq!(elastic_checkpoint_ceiling_ms(1_000, 700, 1_000), 1_000);
    // A 5 s flush venue: P = routine = 5 s ⇒ 2.5 s.
    assert_eq!(elastic_checkpoint_ceiling_ms(5_000, 20, 5_000), 2_500);
    // A reader polling below the writer's routine (the env override):
    // half ITS poll; the physical minimum is one ms.
    assert_eq!(elastic_checkpoint_ceiling_ms(300, 0, 1_000), 150);
    assert_eq!(elastic_checkpoint_ceiling_ms(1, 0, 1_000), 1);
}

/// The symmetric appender REGION's derivations (design-symmetric-metadata
/// §1.6 "Per-appender ring" / "Ring budget per volume", §5.7.3 KD-SYM-10;
/// PR 2): the ring floor is the §4.4 pt 5 checkpoint carve-out plus one
/// max entry rounded to a power of two, the ceiling the solo ring's own
/// derivation, the default two checkpoint ages of the measured commit
/// stream inside them; `appenders_capacity = heap/16 ÷ ring`; the
/// `SQUEEZEFS_SYM_RING_KB` registry range is the floor in KiB to the solo
/// derivation's ceiling; the flush ceiling IS `CHECKPOINT_MAX_AGE_MS`.
/// Drift on any of them is red here.
#[test]
fn sym_appender_ring_derives_from_the_reserve_and_the_solo_ring() {
    use squeezefs::meta_backend::kv::appender::{
        appender_flush_ceiling_ms, appender_flush_ceiling_service_cap_ms,
        appender_ring_bytes_derived, appenders_capacity, ring_budget_bytes, sym_ring_ceiling_bytes,
        SYM_RING_FLOOR_BYTES,
    };
    use squeezefs::meta_backend::kv::checkpoint::{
        checkpoint_tick_period_ms, CHECKPOINT_MAX_AGE_MS,
    };
    use squeezefs::meta_backend::kv::journal::{checkpoint_reserve_bytes, MAX_ENTRY_LEN};
    use squeezefs::meta_backend::kv::superblock::{
        journal_ring_len, JOURNAL_RING_MAX, JOURNAL_RING_MIN,
    };

    // Floor: reserve(256 KiB at this size) + 128 KiB = 384 KiB → 512 KiB.
    assert_eq!(
        SYM_RING_FLOOR_BYTES,
        (checkpoint_reserve_bytes(SYM_RING_FLOOR_BYTES) + MAX_ENTRY_LEN).next_power_of_two()
    );
    assert_eq!(SYM_RING_FLOOR_BYTES, 512 * 1024);
    // Ceiling = the solo ring's clamp, on both canonical shapes.
    let field_vol = 2 * 1024 * GIB; // a 2 TiB metadata volume ⇒ the 32 MiB solo cap
    let floor_vol = 256 * MIB; // a small volume ⇒ the 8 MiB solo floor
    assert_eq!(sym_ring_ceiling_bytes(field_vol), JOURNAL_RING_MAX);
    assert_eq!(sym_ring_ceiling_bytes(floor_vol), JOURNAL_RING_MIN);
    assert_eq!(
        sym_ring_ceiling_bytes(field_vol),
        journal_ring_len(field_vol)
    );
    // Default: 2 × rate × CHECKPOINT_MAX_AGE, clamped — a fresh join (no
    // EWMA) takes the floor; the field peak (8 k creates/s × ≈ 230 B ≈
    // 1.8 MiB/s) lands at ≈ 3.7 MiB; a runaway rate the ceiling.
    assert_eq!(
        appender_ring_bytes_derived(0, field_vol),
        SYM_RING_FLOOR_BYTES
    );
    let peak = 8_000 * 230;
    let expect = 2 * peak * CHECKPOINT_MAX_AGE_MS as u64 / 1000 / 4096 * 4096;
    assert_eq!(appender_ring_bytes_derived(peak, field_vol), expect);
    assert_eq!(
        appender_ring_bytes_derived(u64::MAX / 4, field_vol),
        JOURNAL_RING_MAX
    );
    // Capacity: heap/16 over the ring an appender joins with.
    let heap = 2 * 1024 * GIB;
    assert_eq!(ring_budget_bytes(heap), heap / 16);
    assert_eq!(
        appenders_capacity(heap, SYM_RING_FLOOR_BYTES),
        heap / 16 / SYM_RING_FLOOR_BYTES
    );
    // The knob's registry range ties to the floor and the solo ceiling.
    let knob = squeezefs::env_knobs::lookup("SQUEEZEFS_SYM_RING_KB").expect("registered");
    match knob.kind {
        squeezefs::env_knobs::Kind::Int { lo, hi } => {
            assert_eq!(lo, (SYM_RING_FLOOR_BYTES / 1024) as i128);
            assert_eq!(hi, (JOURNAL_RING_MAX / 1024) as i128);
        }
        other => panic!("SQUEEZEFS_SYM_RING_KB must be Int, got {other:?}"),
    }
    // KD-SYM-10: the flush ceiling is the checkpoint LANDING ceiling of the
    // cadence in force — the trigger plus the two tick-granularity terms
    // the reader's qualify term derives (review round 2, Issue 22: the
    // trigger alone is what the tick fires AT, so a healthy mount's leaves
    // land past it by the pass).
    for interval in [0u64, 50, 5_000] {
        assert_eq!(
            appender_flush_ceiling_ms(interval),
            squeezefs::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_ms(interval)
        );
        assert!(appender_flush_ceiling_ms(interval) > CHECKPOINT_MAX_AGE_MS as u64);
    }
    assert_eq!(
        appender_flush_ceiling_ms(50),
        1_100,
        "the shipped 50 ms flush"
    );
    // PR 13c (F-B1, review round 1 Issue 1c): the audit's SERVICE exclusion
    // is capped at ONE landing ceiling — the contract it excuses against;
    // a hold longer than the ceiling is the stall class its consumers
    // (the free-grace qualify term, the `=0` reader) must see.
    for interval in [0u64, 50, 5_000] {
        assert_eq!(
            appender_flush_ceiling_service_cap_ms(interval),
            appender_flush_ceiling_ms(interval),
            "the service cap is one landing ceiling of the cadence in force"
        );
    }
    // PR 13e (F-B1 — record §7 item 3, the margin derived from the
    // MEASURED cycle term): the cadence TRIGGER in force is the MAX AGE
    // minus the cycle's anticipated landing term, saturating — a term at
    // or past the max age makes a cycle due every tick; the published
    // ceiling itself never widens (a cycle slower than the measured one
    // still trips the audit).
    use squeezefs::meta_backend::kv::checkpoint::checkpoint_trigger_ms;
    for (max_age, term, want) in [
        (
            CHECKPOINT_MAX_AGE_MS as u64,
            0,
            CHECKPOINT_MAX_AGE_MS as u64,
        ),
        (
            CHECKPOINT_MAX_AGE_MS as u64,
            150,
            CHECKPOINT_MAX_AGE_MS as u64 - 150,
        ),
        (
            CHECKPOINT_MAX_AGE_MS as u64,
            CHECKPOINT_MAX_AGE_MS as u64,
            0,
        ),
        (CHECKPOINT_MAX_AGE_MS as u64, 5_000, 0),
        (500, 120, 380),
    ] {
        assert_eq!(
            checkpoint_trigger_ms(max_age, term),
            want,
            "trigger(max age {max_age}, term {term})"
        );
        assert_eq!(
            checkpoint_trigger_ms(max_age, term),
            max_age.saturating_sub(term),
            "the trigger is the max age less the anticipated term, saturating"
        );
    }
    // The trigger's INPUT is the MAX AGE — the age the tick fires AT
    // (`CHECKPOINT_MAX_AGE_MS`, the cadence's and the stats face's word),
    // never the LANDING ceiling `max_age + 2 × tick` (review round 1, Issue
    // 7): fed the max age, a cycle whose term is the anticipated one lands
    // exactly at the ceiling less nothing — `trigger + 2 × tick + term ==
    // ceiling`; fed the landing ceiling it would land `2 × tick` past the
    // promise on every cycle, the margin silently eaten.
    for flush in [0u64, 50, 5_000] {
        let tick = checkpoint_tick_period_ms(flush);
        for term in [0u64, 9, 150, 600] {
            assert_eq!(
                checkpoint_trigger_ms(CHECKPOINT_MAX_AGE_MS as u64, term) + 2 * tick,
                appender_flush_ceiling_ms(flush) - term,
                "flush {flush} ms, term {term} ms: the trigger at the max age lands the \
                 anticipated term inside the landing ceiling"
            );
            assert_eq!(
                checkpoint_trigger_ms(appender_flush_ceiling_ms(flush), term) + 2 * tick,
                appender_flush_ceiling_ms(flush) - term + 2 * tick,
                "fed the landing ceiling the trigger would eat the 2-tick margin"
            );
        }
    }
    // One cycle's landing TERM = its pre-barrier wall + the age decision's
    // lateness past the trigger BEYOND one tick (the wake quantization is
    // the ceiling's first priced tick; the excess — the tick's own device
    // work ahead of its decision — is what the second tick bounds at one
    // period; the tick's wait for the SMO mutex behind another holder is
    // left out of the lateness by the caller).
    use squeezefs::meta_backend::kv::checkpoint::checkpoint_cycle_term_ns;
    let ms = 1_000_000u64;
    let cap = appender_flush_ceiling_ms(50) * ms;
    assert_eq!(checkpoint_cycle_term_ns(60 * ms, 0, 50 * ms, cap), 60 * ms);
    assert_eq!(
        checkpoint_cycle_term_ns(60 * ms, 30 * ms, 50 * ms, cap),
        60 * ms
    );
    assert_eq!(
        checkpoint_cycle_term_ns(60 * ms, 50 * ms, 50 * ms, cap),
        60 * ms
    );
    assert_eq!(
        checkpoint_cycle_term_ns(60 * ms, 132 * ms, 50 * ms, cap),
        142 * ms
    );
    assert_eq!(checkpoint_cycle_term_ns(0, 132 * ms, 50 * ms, cap), 82 * ms);
    // The belt (review round 1, Issue 1): a lateness past one landing
    // ceiling is a stall the audit counts, never a term to anticipate —
    // the fold sees the cap, whatever the decision measured.
    assert_eq!(
        checkpoint_cycle_term_ns(60 * ms, 4_000 * ms, 50 * ms, cap),
        60 * ms + cap - 50 * ms
    );
    assert_eq!(
        checkpoint_cycle_term_ns(u64::MAX, 132 * ms, 50 * ms, cap),
        u64::MAX,
        "saturating"
    );
    // The anticipated term is the MAXIMUM of the samples over the horizon
    // — a bound anticipated by a bound (a mean lands past the promise on
    // every above-mean cycle; a decayed mark leaks inside its memory and
    // lands a burst one step above it a tick short): a burst holds the
    // mark for exactly the horizon and is forgotten when it leaves it. The
    // horizon is the ONE cover-loop bound (`COVER_CYCLES_MAX`).
    use squeezefs::meta_backend::kv::checkpoint::{
        CycleTermWindow, COVER_CYCLES_MAX, TERM_HORIZON_CYCLES,
    };
    assert_eq!(TERM_HORIZON_CYCLES, COVER_CYCLES_MAX as usize);
    let mut w = CycleTermWindow::new();
    assert_eq!(w.anticipated_ns(), 0, "nothing before the first sample");
    w.push(150);
    assert_eq!(w.anticipated_ns(), 150);
    w.push(300);
    assert_eq!(w.anticipated_ns(), 300);
    for _ in 0..TERM_HORIZON_CYCLES - 1 {
        w.push(100);
        assert_eq!(
            w.anticipated_ns(),
            300,
            "the burst holds the mark for the whole horizon"
        );
    }
    w.push(100);
    assert_eq!(
        w.anticipated_ns(),
        100,
        "the burst is forgotten exactly when it leaves the horizon"
    );
}

/// **The cadence's LIVE projection (PR 13g, F-B1)** — the term the
/// horizon cannot carry: a storm's first cycle after a quiet horizon (the
/// box's 199 quiet cycles emptied the 64-cycle window of the previous
/// row's 133 ms and the onset cycle tripped at 1,127 ms with 11 ms
/// anticipated) is priced from the work it CARRIES — the dirty nodes ×
/// the measured per-node append wall + the images the pending commits
/// promised × the measured per-image SMO wall — read live at every tick;
/// the trigger anticipates `max(horizon term, projection)`. A unit is one
/// pass's wall over its count of the class, `None` for a pass that ran
/// none (the unit in force KEEPS what the passes that ran it measured —
/// what survives quiet), and the unit in force is the horizon MAXIMUM
/// over those passes (a bound anticipated by a bound — the term's own
/// law); a unit nothing has measured is 0 (the fresh mount's shipped
/// posture). Drift is red here.
#[test]
fn checkpoint_projection_prices_the_pending_work_from_measured_units() {
    use squeezefs::meta_backend::kv::checkpoint::{
        checkpoint_trigger_ms, flush_unit_ns, projected_flush_wall_ns, CycleTermWindow,
        FlushPassSample, CHECKPOINT_MAX_AGE_MS, TERM_HORIZON_CYCLES,
    };
    let ms = 1_000_000u64;
    // One pass's unit: the wall over the count; none without the class.
    assert_eq!(
        flush_unit_ns(0, 0),
        None,
        "a pass without the class measures nothing"
    );
    assert_eq!(flush_unit_ns(999 * ms, 0), None, "…whatever its wall");
    assert_eq!(flush_unit_ns(80 * ms, 40), Some(2 * ms));
    // The unit in force: the horizon maximum over the passes that ran the
    // class — a slow pass raises it at once, a quiet pass leaves it.
    let mut w = CycleTermWindow::new();
    assert_eq!(w.anticipated_ns(), 0, "nothing measured yet");
    for unit in [
        flush_unit_ns(80 * ms, 40),
        flush_unit_ns(400 * ms, 40),
        flush_unit_ns(0, 0),
    ]
    .into_iter()
    .flatten()
    {
        w.push(unit);
    }
    assert_eq!(
        w.anticipated_ns(),
        10 * ms,
        "the slow pass is the unit in force"
    );
    for _ in 0..TERM_HORIZON_CYCLES - 1 {
        w.push(2 * ms);
    }
    assert_eq!(
        w.anticipated_ns(),
        10 * ms,
        "…for the whole horizon of class passes"
    );
    w.push(2 * ms);
    assert_eq!(
        w.anticipated_ns(),
        2 * ms,
        "…and forgotten when it leaves it"
    );
    // The sample's split by class: a node whose flush wrote images is SMO
    // work, every other an append.
    let mut s = FlushPassSample::default();
    s.note(300_000, 0);
    s.note(200_000, 0);
    s.note(7 * ms, 1);
    s.note(15 * ms, 3);
    assert_eq!((s.nodes, s.node_ns), (2, 500_000));
    assert_eq!((s.smo_nodes, s.images, s.image_ns), (2, 4, 22 * ms));
    // The projection: dirty × per-node + promised × per-image, saturating.
    assert_eq!(projected_flush_wall_ns(0, 250_000, 0, 2 * ms), 0);
    assert_eq!(
        projected_flush_wall_ns(100, 250_000, 40, 2 * ms),
        100 * 250_000 + 40 * 2 * ms
    );
    assert_eq!(
        projected_flush_wall_ns(100, 250_000, 40, 0),
        140 * 250_000,
        "an unmeasured image unit is floored at the node unit — an image is one node write at \
         least"
    );
    assert_eq!(
        projected_flush_wall_ns(100, 250_000, 40, 100_000),
        140 * 250_000,
        "…and so is an image unit measured below it"
    );
    assert_eq!(
        projected_flush_wall_ns(u64::MAX, 2, 1, 1),
        u64::MAX,
        "saturating"
    );
    // The trigger anticipates the LARGER of the horizon term and the
    // projection — the quiet horizon's 0 with 105 ms of pending work fires
    // at 895 ms, never at the shipped 1,000.
    let max_age = CHECKPOINT_MAX_AGE_MS as u64;
    let projected_ms = projected_flush_wall_ns(100, 250_000, 40, 2 * ms) / ms;
    assert_eq!(projected_ms, 105);
    assert_eq!(checkpoint_trigger_ms(max_age, projected_ms), max_age - 105);
    assert_eq!(
        checkpoint_trigger_ms(max_age, 133u64.max(projected_ms)),
        max_age - 133
    );
}

/// **The threshold drain's budget is the PASS's** (PR 13g, F-B1; finding
/// 49's bound restated): the first item of a pass is admitted whatever
/// the deadline — progress — and every later one only while the deadline
/// stands, whichever tree it belongs to, so a pass's bound is `period +
/// one item's service time`. The per-tree form it replaces admitted one
/// free item PER TREE, and a forest of 64 slot trees each with one queued
/// append put the cadence's age decision 350 ms past its trigger at a
/// storm's onset on a parked device — the cycle the projection had priced
/// right. An unbounded budget admits everything.
#[test]
fn threshold_drain_budget_is_the_passes_not_each_trees() {
    use squeezefs::meta_backend::kv::checkpoint::DrainBudget;
    let passed = std::time::Instant::now() - std::time::Duration::from_millis(1);
    let mut b = DrainBudget::new(Some(passed));
    assert!(!b.progressed());
    assert!(
        b.admits(),
        "the pass's first item runs whatever the deadline"
    );
    assert!(b.progressed());
    for _ in 0..64 {
        assert!(
            !b.admits(),
            "past the deadline every later item — every later TREE's first — waits for the next \
             pass"
        );
    }
    let ahead = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    let mut open = DrainBudget::new(Some(ahead));
    for _ in 0..64 {
        assert!(open.admits(), "inside the deadline every item runs");
    }
    let mut unbounded = DrainBudget::unbounded();
    for _ in 0..64 {
        assert!(unbounded.admits(), "the shutdown tick's drain is unbounded");
    }
}

/// **A grant's control entries are bounded by the journal entry cap** (PR
/// 13g review round 1, Issue 3; design §5.3.3 as built): a carve's claim
/// deltas and a return's free deltas pack into entries under
/// `MAX_ENTRY_LEN` BESIDE the rewritten `extent_grant` record — whose
/// frame per chunk is bounded by the record's runs plus the chunk's length
/// (every extent adds at most one run) — and the identity's hint put. The
/// closed form `grant_deltas_per_entry` ≡ the packing's first chunk; every
/// chunk fits; the chunks cover the count contiguously; one more delta
/// would not fit. A 25-byte claim delta packs ≈ 3,500 per entry against an
/// empty record, a 33-byte free delta ≈ 2,900 — the joiner's derived ask
/// at a storm's SMO rate (≈ 8,300) is three entries, where one was
/// `EntryTooLarge` for ever.
#[test]
fn grant_deltas_pack_under_the_journal_entry_cap() {
    use squeezefs::meta_backend::kv::appender::{
        alloc_delta_frame_len, appender_hint_frame_len, free_delta_frame_len,
        grant_deltas_per_entry, pack_grant_deltas,
    };
    use squeezefs::meta_backend::kv::journal::{record_frame_len, ENTRY_HDR_LEN, MAX_ENTRY_LEN};
    use squeezefs::meta_backend::kv::slot_state::extent_grant_frame_len;
    assert_eq!(alloc_delta_frame_len(), record_frame_len(8, 1));
    assert_eq!(alloc_delta_frame_len(), 25);
    assert_eq!(free_delta_frame_len(), record_frame_len(8, 9));
    assert_eq!(free_delta_frame_len(), 33);
    let hint = appender_hint_frame_len();
    let total = |k: u64, delta: u64, runs: usize, side: u64| {
        ENTRY_HDR_LEN + k * delta + extent_grant_frame_len(runs + k as usize) + side
    };
    for (delta, runs, side) in [
        (alloc_delta_frame_len(), 0usize, 0u64),
        (alloc_delta_frame_len(), 1, hint),
        (alloc_delta_frame_len(), 1_000, hint),
        (free_delta_frame_len(), 3, 0),
        (free_delta_frame_len(), 5_000, 0),
    ] {
        let count = 20_000usize;
        let chunks = pack_grant_deltas(count, delta, runs, side);
        assert!(chunks.len() >= 2, "{count} deltas never fit one entry");
        let mut next = 0usize;
        for ch in &chunks {
            assert_eq!(ch.start, next, "the chunks are contiguous");
            assert!(
                total(ch.len() as u64, delta, runs, side) <= MAX_ENTRY_LEN,
                "chunk {ch:?} fits the entry cap"
            );
            next = ch.end;
        }
        assert_eq!(next, count, "the chunks cover the count");
        let per = grant_deltas_per_entry(delta, runs, side);
        assert_eq!(
            chunks[0].len() as u64,
            per,
            "the closed form is the first chunk"
        );
        assert!(
            total(per + 1, delta, runs, side) > MAX_ENTRY_LEN,
            "one more delta would not fit"
        );
    }
    let per_claim = grant_deltas_per_entry(alloc_delta_frame_len(), 1, hint);
    assert!(
        (3_000..4_000).contains(&per_claim),
        "≈ 3,500 claim deltas per entry ({per_claim})"
    );
    let per_free = grant_deltas_per_entry(free_delta_frame_len(), 1, 0);
    assert!(
        (2_500..3_200).contains(&per_free),
        "≈ 2,900 free deltas per entry ({per_free})"
    );
    assert_eq!(
        pack_grant_deltas(8_300, alloc_delta_frame_len(), 1, hint).len(),
        3,
        "the derived ask at 92 SMO/s is three entries"
    );
    assert_eq!(
        pack_grant_deltas(8, alloc_delta_frame_len(), 0, 0).len(),
        1,
        "the floor is one entry"
    );
    assert!(pack_grant_deltas(0, alloc_delta_frame_len(), 0, 0).is_empty());
}

/// **A sized join on a fragmented heap keeps the largest runs that fit the
/// page's table, never refusing** (PR 13g review round 1, Issue 4;
/// `appender::ring_segments_that_fit`): runs within `RING_SEGMENTS_MAX`
/// are kept whole; past it the LARGEST runs that fit HALF the table when
/// they reach the floor (growth keeps its room), else the largest that fit
/// the whole table; kept ascending, the released extents ascending and
/// disjoint, nothing lost. Drift is red here.
#[test]
fn ring_segments_that_fit_keep_the_largest_runs_and_never_refuse() {
    use squeezefs::meta_backend::kv::appender::{
        ring_segments_that_fit, GrantRun, RING_SEGMENTS_MAX,
    };
    let run = |start: u64, len: u32| GrantRun { start, len };
    let floor = 8u64;
    // Within the table: kept whole, nothing released.
    let whole: Vec<GrantRun> = (0..RING_SEGMENTS_MAX as u64)
        .map(|i| run(i * 10, 1))
        .collect();
    let (kept, released) = ring_segments_that_fit(&whole, floor);
    assert_eq!(kept, whole);
    assert!(released.is_empty());
    // Thirty-two one-extent holes (the pin's shape): the four largest are
    // under the floor, so the whole table is spent — eight extents, the
    // floor exactly — and 24 go back.
    let holes: Vec<GrantRun> = (0..32u64).map(|i| run(i * 2, 1)).collect();
    let (kept, released) = ring_segments_that_fit(&holes, floor);
    assert_eq!(kept.len(), RING_SEGMENTS_MAX);
    assert_eq!(kept.iter().map(|r| u64::from(r.len)).sum::<u64>(), floor);
    assert_eq!(released.len(), 24);
    assert!(
        kept.windows(2).all(|w| w[0].start < w[1].start),
        "kept ascending"
    );
    assert!(
        released.windows(2).all(|w| w[0] < w[1]),
        "released ascending"
    );
    for r in &kept {
        assert!(
            !released.contains(&r.start),
            "kept and released are disjoint"
        );
    }
    // Twelve runs where the four largest reach the floor: half the table
    // is spent, growth keeps four slots.
    let mut mixed: Vec<GrantRun> = (0..8u64).map(|i| run(i * 3, 1)).collect();
    mixed.extend([run(100, 4), run(200, 3), run(300, 5), run(400, 2)]);
    mixed.sort_by_key(|r| r.start);
    let (kept, released) = ring_segments_that_fit(&mixed, floor);
    assert_eq!(kept.len(), RING_SEGMENTS_MAX / 2);
    assert_eq!(
        kept.iter().map(|r| r.start).collect::<Vec<_>>(),
        vec![100, 200, 300, 400],
        "the four largest, ascending"
    );
    assert_eq!(released.len(), 8);
    let total_in: u64 = mixed.iter().map(|r| u64::from(r.len)).sum();
    let total_out: u64 = kept.iter().map(|r| u64::from(r.len)).sum::<u64>() + released.len() as u64;
    assert_eq!(total_in, total_out, "nothing lost");
}

/// The symmetric MANAGER's derivations (design-symmetric-metadata §5.3.3
/// grant sizing, §5.9 the failover bound, §1.6 "Manager death"; PR 3):
/// `grant_extents = clamp(2 × ewma_smo_rate × failover_bound_s, 8,
/// free_heap / (4 × appenders))` — the floor is the SMO budget (≤ 2
/// extents per compaction/split × 4 pending root swaps per cycle), the
/// cap a quarter of the free heap over the appenders, never below the
/// floor; `manager_failover_bound_ms = CLIENT_STALE_TTL_SECS × 1000 +
/// ladder + replay` — the two measured terms added to the constant the
/// D0 ladder waits out; the `SQUEEZEFS_SYM_GRANT_EXTENTS` registry range
/// is the floor to the u16 slot namespace's width and the knob wins
/// verbatim. Drift on any of them is red here.
#[test]
fn sym_manager_grant_and_failover_bound_derive_from_the_ladder_and_the_smo_rate() {
    use squeezefs::fuse_client::CLIENT_STALE_TTL_SECS;
    use squeezefs::meta_backend::kv::appender::{
        grant_extents_derived, manager_failover_bound_ms, resolve_grant_extents,
        GRANT_EXTENTS_FLOOR, GRANT_EXTENTS_MAX, SYM_GRANT_EXTENTS_ENV,
    };

    assert_eq!(GRANT_EXTENTS_FLOOR, 2 * 4);
    assert_eq!(GRANT_EXTENTS_MAX, u64::from(u16::MAX) + 1);
    // The bound: the constant TTL plus the two measured walls.
    let bound = manager_failover_bound_ms(CLIENT_STALE_TTL_SECS, 1_000, 500);
    assert_eq!(bound, CLIENT_STALE_TTL_SECS * 1_000 + 1_500);
    // No measured SMO rate ⇒ the floor, at any heap.
    assert_eq!(
        grant_extents_derived(0, bound, 1 << 30, 12),
        GRANT_EXTENTS_FLOOR
    );
    // 5 SMO/s (milli-units) over the bound, doubled.
    let rate_milli = 5_000;
    assert_eq!(
        grant_extents_derived(rate_milli, bound, 1 << 30, 1),
        2 * rate_milli * bound / 1_000_000
    );
    // The cap: a quarter of the free heap over the appenders …
    assert_eq!(
        grant_extents_derived(rate_milli, bound, 4_000, 10),
        4_000 / 40
    );
    // … never below the floor.
    assert_eq!(
        grant_extents_derived(rate_milli, bound, 4, 10),
        GRANT_EXTENTS_FLOOR
    );
    // The knob wins verbatim; the registry range is the floor to the width.
    let knob = squeezefs::env_knobs::lookup(SYM_GRANT_EXTENTS_ENV).expect("registered");
    match knob.kind {
        squeezefs::env_knobs::Kind::Int { lo, hi } => {
            assert_eq!(lo, GRANT_EXTENTS_FLOOR as i128);
            assert_eq!(hi, GRANT_EXTENTS_MAX as i128);
        }
        other => panic!("SQUEEZEFS_SYM_GRANT_EXTENTS must be Int, got {other:?}"),
    }
    std::env::set_var(SYM_GRANT_EXTENTS_ENV, "64");
    assert_eq!(resolve_grant_extents(0, bound, 1 << 30, 1), 64);
    std::env::remove_var(SYM_GRANT_EXTENTS_ENV);
    assert_eq!(
        resolve_grant_extents(0, bound, 1 << 30, 1),
        GRANT_EXTENTS_FLOOR
    );
}

/// PR 4's slot-lease derivations (design-symmetric-metadata §5.1.2 /
/// §5.1.4, §6.1): `M = clamp(W / (2 × writers_known), 1, MINT_SPREAD)` —
/// 64 on a solo mount at the derived width, 2 at the 12,500-writer
/// operating point, never 0; `A_max = max(used_leaf_bytes / MINT_SPREAD,
/// node_size)` with the static knob clamped to `[node_size, heap]`;
/// `N_floor = max(2, ceil(handover / ship))`; the cold-start handover cost
/// is four barriers + three ship round trips; `T_idle` = `T_owner`
/// (`SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS`, the 45 s TTL) unless explicit;
/// the four registry ranges tie to the constants and every knob wins
/// verbatim. Drift on any of them is red here.
#[test]
fn sym_slot_lease_rotor_ceiling_floor_and_window_derive_from_the_width_and_the_lease() {
    use squeezefs::fuse_client::CLIENT_STALE_TTL_SECS;
    use squeezefs::meta_backend::kv::slot_lease::{
        affinity_ceiling_in_force, mint_slots_in_force, resolve_affinity_ceiling,
        resolve_mint_slots, resolve_t_idle_ms, SYMMETRIC_META_ENV, SYM_AFFINITY_MAX_MB_ENV,
        SYM_MINT_SLOTS_ENV, SYM_T_IDLE_MS_ENV,
    };
    use squeezefs::meta_backend::DERIVED_ROUTING_WIDTH;
    use squeezefs::slot_lease_core::{
        affinity_ceiling_bytes, handover_cold_start_ns, mint_slots_derived, n_floor, MINT_SPREAD,
    };
    let w = u64::from(DERIVED_ROUTING_WIDTH);
    assert_eq!(MINT_SPREAD, squeezefs::meta_backend::MINT_SPREAD as u64);
    assert_eq!(mint_slots_derived(w, 1), MINT_SPREAD, "a solo mount: 64");
    assert_eq!(
        mint_slots_derived(w, 0),
        MINT_SPREAD,
        "a census of 0 reads as 1"
    );
    assert_eq!(
        mint_slots_derived(w, 512),
        MINT_SPREAD,
        "the ceiling binds to 512 writers"
    );
    assert_eq!(mint_slots_derived(w, 1_024), 32);
    assert_eq!(mint_slots_derived(w, 12_500), 2, "the operating point");
    assert_eq!(mint_slots_derived(w, 1 << 20), 1, "never 0");
    // A_max: load-relative, never below one extent; the static knob is
    // clamped to [node_size, heap].
    let node = 256 * 1024u64;
    assert_eq!(affinity_ceiling_bytes(0, node), node);
    assert_eq!(affinity_ceiling_bytes(64 * node, node), node);
    assert_eq!(affinity_ceiling_bytes(6_400 * node, node), 100 * node);
    let heap = 1 << 30;
    assert_eq!(
        affinity_ceiling_in_force(None, 6_400 * node, node, heap),
        100 * node
    );
    assert_eq!(affinity_ceiling_in_force(Some(1), 0, node, heap), 1 << 20);
    assert_eq!(
        affinity_ceiling_in_force(Some(1), 0, 4 << 20, heap),
        4 << 20,
        "≥ node_size"
    );
    assert_eq!(
        affinity_ceiling_in_force(Some(4096), 0, node, heap),
        heap,
        "≤ heap"
    );
    // N_floor and the cold start.
    assert_eq!(n_floor(0, 0), 2);
    assert_eq!(n_floor(10, 3), 4);
    assert_eq!(n_floor(1_000, 0), 1_000, "a ship of 0 ns reads as 1");
    assert_eq!(handover_cold_start_ns(100, 10), 430);
    // The registry ranges tie to the constants.
    let lookup = |k: &str| squeezefs::env_knobs::lookup(k).expect("registered");
    assert!(matches!(
        lookup(SYMMETRIC_META_ENV).kind,
        squeezefs::env_knobs::Kind::Bool
    ));
    match lookup(SYM_MINT_SLOTS_ENV).kind {
        squeezefs::env_knobs::Kind::Int { lo, hi } => {
            assert_eq!((lo, hi), (1, MINT_SPREAD as i128));
        }
        other => panic!("SQUEEZEFS_SYM_MINT_SLOTS must be Int, got {other:?}"),
    }
    match lookup(SYM_AFFINITY_MAX_MB_ENV).kind {
        squeezefs::env_knobs::Kind::Int { lo, hi } => {
            assert_eq!(lo, 1);
            assert_eq!(hi, 1 << 20, "BYTES_MAX (1 TiB) in MiB");
        }
        other => panic!("SQUEEZEFS_SYM_AFFINITY_MAX_MB must be Int, got {other:?}"),
    }
    match lookup(SYM_T_IDLE_MS_ENV).kind {
        squeezefs::env_knobs::Kind::Int { lo, hi } => {
            assert_eq!((lo, hi), (1_000, 600_000));
        }
        other => panic!("SQUEEZEFS_SYM_T_IDLE_MS must be Int, got {other:?}"),
    }
    // Every knob wins verbatim.
    for k in [
        SYM_MINT_SLOTS_ENV,
        SYM_AFFINITY_MAX_MB_ENV,
        SYM_T_IDLE_MS_ENV,
    ] {
        std::env::remove_var(k);
    }
    std::env::remove_var("SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS");
    assert_eq!(resolve_mint_slots(w, 1), MINT_SPREAD);
    assert_eq!(mint_slots_in_force(Some(7), w, 1), 7);
    std::env::set_var(SYM_MINT_SLOTS_ENV, "7");
    assert_eq!(resolve_mint_slots(w, 1), 7);
    std::env::remove_var(SYM_MINT_SLOTS_ENV);
    assert_eq!(
        resolve_affinity_ceiling(6_400 * node, node, heap),
        100 * node
    );
    std::env::set_var(SYM_AFFINITY_MAX_MB_ENV, "2");
    assert_eq!(resolve_affinity_ceiling(6_400 * node, node, heap), 2 << 20);
    std::env::remove_var(SYM_AFFINITY_MAX_MB_ENV);
    assert_eq!(
        resolve_t_idle_ms(),
        CLIENT_STALE_TTL_SECS * 1_000,
        "T_owner"
    );
    std::env::set_var(SYM_T_IDLE_MS_ENV, "1500");
    assert_eq!(resolve_t_idle_ms(), 1_500);
    std::env::remove_var(SYM_T_IDLE_MS_ENV);
}

/// Symmetric metadata PR 8 (design-symmetric-metadata §5.5 / §5.5.1 /
/// §5.5.3): the ranged block grant `G = clamp(2 × ewma × T_renewal, 64,
/// cap / (2 × writers))` — floor 64 = one write-pipeline BDP window of
/// 4 MiB blocks, cap = half the volume spread over the writers; the data
/// bitmap's 32 KiB per TiB at the shipped 4 MiB block; `T_park_max =
/// manager_failover_bound_ms + grace_ms` — every term re-derived here
/// from the constants it names.
#[test]
fn sym_block_grant_bitmap_and_park_bound_derive_from_their_terms() {
    use squeezefs::block_grant::{block_grant_derived, BLOCK_GRANT_FLOOR};
    use squeezefs::data_alloc_bitmap::{bitmap_bytes_for, pages_for, DATA_ALLOC_PAGE_BITS};
    use squeezefs::park_gate::t_park_max_ms;
    assert_eq!(BLOCK_GRANT_FLOOR, 64, "one BDP window of 4 MiB blocks");
    let cap_blocks = (1u64 << 40) / (4 << 20);
    // Below the floor's worth of rate the floor answers.
    assert_eq!(
        block_grant_derived(0, 10_000, cap_blocks, 1),
        BLOCK_GRANT_FLOOR
    );
    // 2 × 100 blocks/s × 10 s = 2,000.
    assert_eq!(block_grant_derived(100_000, 10_000, cap_blocks, 1), 2_000);
    // The cap: half the volume over the writers.
    assert_eq!(
        block_grant_derived(u64::MAX / 4, 10_000, cap_blocks, 8),
        cap_blocks / 16
    );
    // A cap below the floor is the floor (the carve truncates at what is
    // free).
    assert_eq!(
        block_grant_derived(u64::MAX / 4, 10_000, 32, 8),
        BLOCK_GRANT_FLOOR
    );
    assert_eq!(bitmap_bytes_for(1 << 40, 4 << 20), 32 * 1024);
    assert_eq!(
        bitmap_bytes_for(1 << 50, 4 << 20),
        32 << 20,
        "32 MiB per PiB"
    );
    assert_eq!(
        bitmap_bytes_for(1 << 40, 0),
        0,
        "a zero block size divides nothing"
    );
    assert_eq!(DATA_ALLOC_PAGE_BITS, (4096 - 32) * 8);
    assert_eq!(pages_for(DATA_ALLOC_PAGE_BITS), 1);
    assert_eq!(pages_for(DATA_ALLOC_PAGE_BITS + 1), 2);
    assert_eq!(t_park_max_ms(46_500, 45_000), 91_500);
    assert_eq!(t_park_max_ms(u64::MAX, 1), u64::MAX, "saturating");
    assert!(
        t_park_max_ms(30_000, 0) >= 30_000,
        "T_park_max is never below SQUEEZEFS_TIMEOUT's shipped 30 s when the failover bound is"
    );
    // Review round 1, Issue 16: the grace term IS `LeaseClocks::grace` (the
    // S6 owner-failover window), read off the derivation — never `t_owner`
    // standing in for it. And the term the arm passes is that field.
    let clocks = squeezefs::membership::LeaseClocks::derive(std::time::Duration::ZERO)
        .expect("shipped clocks");
    assert_eq!(
        squeezefs::park_gate::t_park_max_for(1_234, &clocks),
        1_234 + clocks.grace.as_millis() as u64
    );
    // Review round 1, Issue 17: the beat's fallback is the shipped
    // heartbeat's physical default, named — never a free literal.
    assert_eq!(
        squeezefs::membership::RENEWAL_BEAT_FALLBACK_MS,
        squeezefs::fuse_client::CLIENT_HEARTBEAT_INTERVAL_SECS * 1000
    );
    assert!(squeezefs::membership::renewal_beat_ms() > 0);
}

/// PR 5's read-token derivations (design-symmetric-metadata §5.7; review
/// round 1, Issue 7): the reader's token RECORDS budget is 1/256 of the
/// R5 memory budget floored at ONE control frame (the physical minimum
/// under which a grant page can be resident at all — no token could ever
/// be served below it), never an entry count; the grant's dentry page
/// budget is half the control frame cap; the token lane's recall
/// deadline is the membership lease TTL (`SQUEEZEFS_MEMBERSHIP_LEASE_TTL_
/// MS`, 45 s shipped) — never a measured p99 — and the recall channel's
/// park is a quarter of it inside the S10 park's floor and one second.
/// Drift on any of them is red here.
#[test]
fn sym_read_token_budget_page_and_deadline_derive_from_r5_the_frame_cap_and_the_lease() {
    use squeezefs::cluster_wire::CONTROL_MAX_FRAME_BYTES;
    use squeezefs::meta_ship::token_plane::grant_dentry_budget;
    use squeezefs::meta_ship::tokens::{
        records_budget_bytes, RecallLane, RECALL_POLL_PARK_FLOOR, RECORDS_BUDGET_DIVISOR,
    };
    let budget = squeezefs::mem_budget::MEM_BUDGET.budget_bytes();
    assert_eq!(RECORDS_BUDGET_DIVISOR, 256);
    assert_eq!(
        records_budget_bytes(),
        (budget / RECORDS_BUDGET_DIVISOR).max(u64::from(CONTROL_MAX_FRAME_BYTES)),
        "the records budget is 1/256 of R5, floored at one control frame"
    );
    assert_eq!(
        grant_dentry_budget(),
        (CONTROL_MAX_FRAME_BYTES / 2) as usize,
        "a grant page's dentries take half the frame cap"
    );
    let ttl = squeezefs::membership::LeaseClocks::derive(std::time::Duration::ZERO)
        .expect("shipped clocks")
        .t_owner;
    assert_eq!(
        RecallLane::live_tokens().config().deadline,
        ttl,
        "the token lane's recall deadline IS the membership lease TTL"
    );
    assert!(RECALL_POLL_PARK_FLOOR <= std::time::Duration::from_secs(1));
}

/// Symmetric PR 9 (review round 3, Issues 24/25): the custody handover's
/// parks and bounds are functions of the S9 lease clocks — the writer's
/// deferred-retry / settle park is one twentieth of the renewal beat, the
/// mid-handover mark stands two beats, the holder's clean leave waits
/// `T_owner + renew` for its recalled grants (the S9 sweep's bound) —
/// at the shipped clocks and at the contracts' 1 s beat alike.
#[test]
fn sym_custody_handover_park_and_bounds_derive_from_the_lease_clocks() {
    use squeezefs::data_grant::{
        handover_recall_bound_for, handover_retry_park_for, leave_custody_bound_for,
    };
    use std::time::Duration;
    let shipped =
        squeezefs::membership::LeaseClocks::derive(Duration::ZERO).expect("shipped clocks");
    for renew in [shipped.renew_interval, Duration::from_secs(1)] {
        assert_eq!(
            handover_retry_park_for(renew),
            (renew / 20).max(Duration::from_millis(1)),
            "the retry / settle park is renew / 20"
        );
        assert_eq!(
            handover_recall_bound_for(renew),
            renew * 2,
            "the mid-handover mark stands two beats"
        );
    }
    assert_eq!(
        leave_custody_bound_for(shipped.t_owner, shipped.renew_interval),
        shipped.t_owner + shipped.renew_interval,
        "the leave waits T_owner + renew for its recalled grants"
    );
    assert!(
        handover_retry_park_for(shipped.renew_interval)
            < handover_recall_bound_for(shipped.renew_interval),
        "many retries fit one mark"
    );
}

/// PR 10's recovery time bound (design-symmetric-metadata §5.9 / §11): a
/// dead region's window of `ring_bytes` holds at most `ring / 230 B`
/// entries (the measured journal entry per create, §1.4), clustered into
/// at most `entries / (800 × node_size / 256 KiB)` leaves — each ONE cold
/// leaf load at the measured 120 µs upper end — plus 1 µs of fold per
/// entry and ONE barriered checkpoint cycle (the landing ceiling of the
/// cadence in force). Monotone in the ring, scaled by the node size, and
/// the same landing-ceiling derivation the flush ceiling rides. Drift on
/// any term is red here; the published per-volume gauge is 0 on a flat
/// volume (no region can die there).
#[test]
fn sym_appender_recovery_bound_derives_from_the_ring_the_node_size_and_the_landing_ceiling() {
    use squeezefs::meta_backend::kv::backend::appender_recovery_bound_ms;
    use squeezefs::meta_backend::kv::checkpoint::checkpoint_landing_ceiling_ms;
    let node = 256 * 1024u64;
    for (ring, flush) in [
        (512u64 * 1024, 50u64),
        (1 << 20, 50),
        (32 << 20, 0),
        (32 << 20, 5_000),
    ] {
        let entries = ring / 230;
        let leaves = entries.div_ceil(800);
        // Three leaf passes (review round 2, Issue 10): the flush's cold
        // loads, the tail scan's extent reads, the orphan census's
        // interior reads.
        let expected = leaves.saturating_mul(120 * 3).div_ceil(1_000)
            + entries.saturating_mul(1_000).div_ceil(1_000_000)
            + checkpoint_landing_ceiling_ms(flush);
        assert_eq!(
            appender_recovery_bound_ms(ring, node, flush),
            expected,
            "ring {ring} flush {flush}: three leaf passes + fold + one landing ceiling"
        );
    }
    // Monotone in the ring; a smaller node holds fewer files per leaf and
    // costs more loads for the same window.
    assert!(
        appender_recovery_bound_ms(32 << 20, node, 50)
            > appender_recovery_bound_ms(1 << 20, node, 50)
    );
    assert!(
        appender_recovery_bound_ms(32 << 20, 64 * 1024, 50)
            > appender_recovery_bound_ms(32 << 20, node, 50)
    );
    // The shipped shape: a 32 MiB ring at 256 KiB nodes under the 50 ms
    // cadence — ≈ 146 k entries, 183 leaves × 3 passes, 66 + 146 + 1,100 ms.
    assert_eq!(
        appender_recovery_bound_ms(32 << 20, node, 50),
        66 + 146 + 1_100
    );
}

/// **PR 10, review round 8, Issue 36 — the death-path custody quarantine's
/// bound is `T_owner + 2 × skew_max` of the custody lease clocks.** The
/// OWNER's view: `T_owner` is the instant past which a holder may re-grant
/// a lease it issued (the member's `T_self = T_owner − 2·skew_max −
/// D_purge` is the STRICTER self-fence — the wrong side for an owner-side
/// quarantine), and the death record's `ts_ms` is the RECORDER's wall
/// clock compared against the quarantining node's, so the bound absorbs
/// `2 × skew_max` of disagreement. With no custody authority installed the
/// process-wide bound is the shipped derivation's; the bound outlives no
/// death record (`death_record_retire_age_ms` = 2 × T_owner ≥ it, since
/// `2·skew_max < T_owner` by the clocks' own admissibility).
#[test]
fn sym_custody_quarantine_bound_derives_from_t_owner_and_skew_max() {
    use squeezefs::data_grant::{custody_quarantine_bound_for, custody_quarantine_bound_ms};
    use squeezefs::membership::LeaseClocks;
    use squeezefs::meta_backend::kv::alloc_lease::death_record_retire_age_ms;
    use std::time::Duration;
    for (t_owner, skew, purge) in [
        (3_000u64, 200u64, 400u64),
        (45_000, 22, 1_100),
        (600, 100, 100),
    ] {
        let clocks = LeaseClocks::with_params(
            Duration::from_millis(t_owner),
            Duration::from_millis(skew),
            Duration::from_millis(purge),
        )
        .expect("admissible clocks");
        assert_eq!(
            custody_quarantine_bound_for(&clocks),
            t_owner + 2 * skew,
            "T_owner {t_owner} skew {skew}: the owner-side bound"
        );
        assert!(
            custody_quarantine_bound_for(&clocks) > clocks.t_self.as_millis() as u64,
            "strictly past the member's self-fence"
        );
        assert!(
            custody_quarantine_bound_for(&clocks) <= 2 * t_owner,
            "never past the record's retirement age"
        );
    }
    squeezefs::data_grant::uninstall_custody_owner();
    let shipped = LeaseClocks::derive(Duration::ZERO).expect("the shipped clocks derive");
    assert_eq!(
        custody_quarantine_bound_ms(),
        custody_quarantine_bound_for(&shipped),
        "no authority installed: the shipped derivation's bound"
    );
    assert!(
        custody_quarantine_bound_ms()
            <= death_record_retire_age_ms(shipped.t_owner.as_millis() as u64)
    );
}

/// **PR 10, review round 2, Issue 27 — the death record's retirement age
/// is ONE law with two readers.** `death_record_retire_age_ms` = TWO lease
/// TTLs (one for every manager's poll to project and act, one for the
/// successor's grace window — a reclaimer's re-join must never race a
/// retired record); the poll's sweep reads it, and the S6 owner's
/// departed-key memo — the `RecordDeath` key-word screen's witness for a
/// member no longer in the census — keeps a departure's key EXACTLY that
/// long: at the age the key still answers, one tick past it the memo
/// answers nothing (the record it would have judged is retirable). The
/// memo is bounded by age, never a count — every entry was a census
/// member whose state the owner already held.
#[test]
fn sym_death_record_retire_age_ties_the_sweep_and_the_departed_key_memo() {
    use squeezefs::membership::{
        JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
    };
    use squeezefs::meta_backend::kv::alloc_lease::{
        death_record_retire_age_ms, DEATH_RECORD_RETIRE_TTLS,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    assert_eq!(DEATH_RECORD_RETIRE_TTLS, 2);
    assert_eq!(death_record_retire_age_ms(45_000), 90_000);
    assert_eq!(death_record_retire_age_ms(u64::MAX), u64::MAX, "saturating");
    let clocks = LeaseClocks::derive(Duration::from_micros(250)).expect("the shipped derivation");
    let t_owner_ms = clocks.t_owner.as_millis() as u64;
    let age = death_record_retire_age_ms(t_owner_ms);
    let ticks = Arc::new(AtomicU64::new(1_000));
    let owner = MembershipOwner::arm(
        "owner-tie",
        3,
        2,
        clocks,
        LeaseClock::manual(Arc::clone(&ticks)),
    )
    .expect("arm");
    let key = 0x5100_0000_0000_0017u64;
    match owner.join(JoinRequest {
        id: "m17".to_string(),
        role: MemberRole::Writer,
        endpoint: Some("10.0.0.7:7100".to_string()),
        pid: std::process::id(),
        boot: "boot-test".to_string(),
        prior_epoch: None,
        pr_key: key,
        mount: None,
    }) {
        JoinOutcome::Granted(_) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(
        owner.registered_key("m17"),
        Some(key),
        "the live census's key"
    );
    owner.evict("m17", "tie test").expect("evicted");
    assert_eq!(
        owner.registered_key("m17"),
        Some(key),
        "the departed memo answers the key at the departure"
    );
    ticks.fetch_add(age, Ordering::SeqCst);
    assert_eq!(
        owner.registered_key("m17"),
        Some(key),
        "at the retirement age the record may still stand — the key still answers"
    );
    ticks.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        owner.registered_key("m17"),
        None,
        "one tick past the retirement age the memo answers nothing"
    );
}

/// PR 12b review round 2, Issue 27: the fleet fsck collect loop's progress
/// deadline is `max(shard lease TTL, 4 × shard 0's wall, FLOOR)`, and the
/// FLOOR is the job wire's own dial/handshake deadline — a worker's
/// proposal is one wire round trip, so "late" cannot be judged below the
/// bound the wire grants a single dial (a shard 0 that finished in
/// microseconds on an empty set must not read a worker still inside its
/// first round trip as wedged). The floor carried no reason before; a
/// drift between the two constants is red here.
#[test]
fn fleet_collect_progress_floor_is_the_job_wires_dial_deadline() {
    assert_eq!(
        squeezefs::fsck::fleet_collect_progress_floor(),
        squeezefs::job_wire::ENROLL_DIAL_TIMEOUT,
        "the collect loop's floor is the wire's dial deadline, never a free-floating second"
    );
    assert!(
        squeezefs::fsck::fleet_collect_progress_floor() <= squeezefs::job_wire::LEASE_TTL,
        "the floor never outranks the shipped shard lease TTL — it binds only where a harness shortens the TTL"
    );
}

/// **The node-seq incarnation space** (PR 13, `kv::node_seq`): `K = 38`
/// bits of mints per incarnation and `63 − K = 25` bits of incarnations
/// partition the 63 usable bits above the volume base's cleared top bit —
/// a drift in either constant is red here, and the arithmetic the module
/// doc states (2^38 ≈ 2.7 × 10^11 mints — 200 SMOs/s for 43 years; 2^25
/// ≈ 33.5 M incarnations — 15 k mounts re-joining daily for six years)
/// is asserted as the bounds it derives from, never as free constants.
#[test]
fn node_seq_incarnation_space_partitions_the_63_usable_bits() {
    use squeezefs::meta_backend::kv::node_seq::{
        incarnation_base, INCARNATION_ORDINAL_BITS, INCARNATION_ORDINAL_MAX, INCARNATION_SPACE,
        INCARNATION_SPACE_BITS,
    };
    assert_eq!(INCARNATION_SPACE_BITS + INCARNATION_ORDINAL_BITS, 63);
    assert_eq!(INCARNATION_SPACE, 1u64 << INCARNATION_SPACE_BITS);
    // Mints per incarnation cover 43 years at the storm rows' 200 SMOs/s.
    let smos_per_s = 200u64;
    let years_43 = 43 * 365 * 86_400 * smos_per_s;
    assert!(
        INCARNATION_SPACE > years_43,
        "{INCARNATION_SPACE} vs {years_43}"
    );
    // Incarnations cover 15 k mounts re-joining daily for six years.
    let joins_6y = 15_000u64 * 365 * 6;
    assert!(INCARNATION_ORDINAL_MAX > joins_6y);
    // Every base below 2^63 admits every ordinal up to the max; the max + 1
    // and any base with its top bit set refuse.
    let top = u64::MAX >> 1;
    assert!(incarnation_base(top, INCARNATION_ORDINAL_MAX).is_some());
    assert!(incarnation_base(top, INCARNATION_ORDINAL_MAX + 1).is_none());
    // The builder's base keeps the top bit clear (the 2^63 headroom law).
    let b = squeezefs::meta_backend::kv::builder::node_seq_base([0xff; 16]);
    assert_eq!(b >> 63, 0);
    assert!(incarnation_base(b, INCARNATION_ORDINAL_MAX).is_some());
}

/// **The membership re-assertion park's bound** (symmetric PR 13b,
/// §4.4ag — `membership::reassertion_wait_bound`): a token fetch refused
/// "not a member" by a successor parks for exactly TWO renewal beats —
/// the member's reclaim loop paces at the beat (one beat to its next
/// attempt) and the successor's re-assertion half admits it inside the
/// next (its window is `T_owner ≥` the beat) — then surfaces the
/// retryable class, never `EIO`. The bound is the beat's derivation, never
/// a free constant: a drift between the two is red here (review round 1,
/// Issue 5).
#[test]
fn membership_reassertion_wait_bound_is_two_renewal_beats() {
    let beat_ms = squeezefs::membership::renewal_beat_ms();
    assert!(beat_ms > 0);
    assert_eq!(
        squeezefs::membership::reassertion_wait_bound(),
        std::time::Duration::from_millis(beat_ms * 2),
        "the re-assertion park is exactly 2 × renewal_beat_ms ({beat_ms} ms)"
    );
}
