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
