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
// A9 — IPC per-session arena default: fraction of the admission cap
// ---------------------------------------------------------------------------

/// The flat 64 MiB `SQUEEZEFS_IPC_ARENA_MB` default becomes
/// `max(64 MiB, dma_align_down(cap/128))`: cap/128 = the per-uid session
/// cap (64) × 2 safety — even a full per-uid population of default-size
/// arenas fits in half the admission cap; alignment is the SLOT-SLAB DMA
/// law (slots × 4 KiB = 4 MiB — also a PMD multiple, so the THP collapse
/// law `map_shared_pmd_aligned` still holds); floor 64 MiB = the shipped
/// posture. Env stays absolute-verbatim (MiB).
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
    // Field: cap = 22 GiB ⇒ 176 MiB per session (4 MiB-aligned).
    assert_eq!(
        resolve_ipc_arena_bytes(None, mem_budget::ipc_arena_cap(FIELD_BUDGET)),
        176 * MIB
    );
    // Floor shape: cap = 358 MiB ⇒ 2.8 MiB < 64 MiB ⇒ shipped floor.
    assert_eq!(
        resolve_ipc_arena_bytes(None, mem_budget::ipc_arena_cap(FLOOR_BUDGET)),
        64 * MIB
    );
    // DMA alignment: a cap that derives to a non-4 MiB multiple rounds
    // DOWN (never over the cap fraction).
    assert_eq!(resolve_ipc_arena_bytes(None, 129 * 128 * MIB), 128 * MIB);
    // THE BOUNCE SHAPE: cap/128 = 90 MiB — an odd 2 MiB multiple. The
    // pre-fix PMD round-down kept 90 MiB (slab 92,160 B ≡ 2048 mod 4096
    // ⇒ 50 % of slots DMA-ineligible); the DMA law rounds to 88 MiB.
    assert_eq!(
        resolve_ipc_arena_bytes(None, 90 * 128 * MIB),
        88 * MIB,
        "an odd 2 MiB-multiple arena is the 50 %-bounce shape — the \
         derivation must round to the slot-slab DMA alignment"
    );
    // The derivation-wide invariant: every derived arena is slot-slab
    // DMA-aligned (slots × 4 KiB).
    let align = u64::from(squeezefs_ipc::layout::Geometry::default_v1().slots) * 4096;
    assert_eq!(align, 4 * MIB, "geometry drift — re-derive the arena alignment");
    for cap_mib in [1_u64, 300, 8192, 11_520, 22_528, 90 * 128, 129 * 128] {
        let got = resolve_ipc_arena_bytes(None, cap_mib * MIB);
        assert_eq!(
            got % align,
            0,
            "derived arena {got} for cap {cap_mib} MiB is not slot-slab DMA-aligned"
        );
    }
    // Env verbatim (MiB), incl. the A0 lever.
    assert_eq!(resolve_ipc_arena_bytes(Some("64"), 22 * GIB), 64 * MIB);
    assert_eq!(
        resolve_ipc_arena_bytes(Some("junk"), FLOOR_BUDGET),
        64 * MIB
    );
    // An EXPLICIT odd env value stays verbatim (explicit-wins law) — the
    // CLIENT slab law (`Geometry::slot_slab`) is what keeps its slots
    // DMA-eligible; see `slot_slab_is_dma_aligned` below.
    assert_eq!(resolve_ipc_arena_bytes(Some("90"), 22 * GIB), 90 * MIB);
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
