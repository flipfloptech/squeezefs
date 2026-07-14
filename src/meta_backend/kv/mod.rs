//! CoW KV metadata node layer (MetaLV v3) — pure in-memory building blocks.
//!
//! PR K1 of `docs/design-cow-kv-metadata.md`: memcmp-ordered key encodings
//! for the three logical trees, record framing with the `Put`/`Delta`/`Delete`
//! kinds and the **single fold algebra** shared byte-identically by point
//! lookup, bset n-way merge, compaction, and journal replay (design §4.2),
//! xxh3-checksummed bset build/verify/binary-search/merge (§4.3), the seeded
//! dentry/xattr hash + `coll_seq` collision scheme, and the readdir cookie
//! contract (§5.1).
//!
//! PR K2 adds [`node`]: the on-disk CoW btree node format — header page,
//! bset-frame appends, the §4.5 positional torn-tail classifier, and
//! compact/split as pure functions over caller-provided extents, with all
//! extent I/O through `crate::uring_fs` (io_uring-only).
//!
//! PR K3 adds the journal ring: the pure lock-free reservation/admission
//! core ([`journal_core`], loom-modeled), page/entry framing + replay scan
//! over `crate::uring_fs` ([`journal`], §4.1/§4.4), and the A/B root-ledger
//! records ([`checkpoint`], §4.1 — records only; scheduling is PR K6b).
//!
//! PR K4 adds the extent allocator (§4.7): the pure lock-free bitmap /
//! pending-free / reserve core ([`alloc_ext_core`], loom-modeled) and the
//! A/B bitmap pages + journaled alloc/free deltas + typed ENOSPC surface
//! ([`alloc_ext`]).
//!
//! PR K5 adds the btree over a RAM-authoritative node cache: the pure
//! lock-free node lifecycle core ([`node_state_core`], loom-modeled —
//! clean/dirty/serializing/superseded, freeze-swap vs apply, supersede vs
//! revalidate), demand paging with arc-swap immutable snapshots /
//! latch-free reads / clock eviction / single-flight loads / the cache
//! budget knob ([`node_cache`], §4.5), and lookup/insert/delete/range with
//! the §4.6 SMO protocol — serialized SMO execution, interior locks
//! SMO-only, writer lock-then-revalidate-then-retry ([`tree`]).
//!
//! PR K6a wires the mount path: [`superblock`] (SuperblockV3 + the
//! sector-0 version gate, §4.1/§6.1, resolved OQ 1 ring clamp),
//! [`builder`] (the offline bulk image builder — §8 gate-volume producer
//! — plus the public v3 formatter and the §4.10 digest walk), and
//! [`backend`] (`KvMetaBackend`, the read side: mount =
//! SB → ledger → bitmap → read-only journal replay into the K5 cache;
//! lookups/getattr/readdir/getxattr/listxattr over tree + fold). The
//! §4.4 commit pipeline, checkpoint scheduling, and the mutating
//! `Metadata` impl are PR K6b's.
//!
//! (PR K9's offline v2 → v3 `migrate` converter lived here until v2
//! support was removed entirely — v3 is the only metadata format.)

pub mod alloc_ext;
pub mod alloc_ext_core;
pub mod backend;
pub mod bset;
pub mod builder;
pub mod checkpoint;
pub mod journal;
pub mod journal_core;
pub mod node;
pub mod node_cache;
pub mod node_state_core;
pub mod record;
pub mod superblock;
pub mod tree;

use std::sync::atomic::AtomicU64;

/// Dentry hash-collision chain exhaustion events (design §4.2): a 257th
/// same-`(parent, hash54)` name found every `coll_seq` occupied, so the
/// insert was rejected with [`KvError::DentryChainOverflow`]. Surfaced on the
/// stats inode as `meta_kv_dentry_collision_overflows` when K6a wires the
/// mount path; until then it is read by this module's tests.
pub static META_KV_DENTRY_COLLISION_OVERFLOWS: AtomicU64 = AtomicU64::new(0);

/// `Delta`-without-base folds (design §4.2): delta records discarded because
/// the scan exhausted all sources without finding an underlying `Put` or a
/// shadowing `Delete` — a counted no-op reachable only through replay
/// orderings. Surfaced as `meta_kv_delta_orphans` when K6a wires the mount
/// path; until then it is read by this module's tests.
pub static META_KV_DELTA_ORPHANS: AtomicU64 = AtomicU64::new(0);

/// Torn/garbage tail bsets dropped by the §4.5 positional node-load
/// classifier — the expected un-checkpointed-tail artifact (every truncated
/// record has seq > the durable tail; replay re-supplies it). Nonzero after
/// a **clean** unmount is the corruption alert, mirroring
/// `meta_kv_replay_dropped_torn` (design §10). Surfaced on the stats inode
/// as `meta_kv_node_dropped_tail_bsets` when K6a wires the mount path; until
/// then it is read by the node-layer tests and the crash harness.
pub static META_KV_NODE_DROPPED_TAIL_BSETS: AtomicU64 = AtomicU64::new(0);

/// Node-cache hits: latch-free map reads served from an arc-swap snapshot
/// (design §4.5/§10 — the Stat-regression early signal). Stats-JSON wiring
/// as `meta_kv_node_cache_hits` is PR K6a/K7's; until then the tree tests
/// read it.
pub static META_KV_NODE_CACHE_HITS: AtomicU64 = AtomicU64::new(0);

/// Node-cache misses: demand-page loads actually performed (single-flight
/// collapses racing callers onto one load — the losers re-check the map
/// and count as hits). Surfaced as `meta_kv_node_cache_misses` in K6a/K7.
pub static META_KV_NODE_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

/// Clock evictions of clean, unpinned nodes (design §4.5: dirty nodes are
/// pinned until writeback; interior nodes and roots pinned uncondition-
/// ally). Surfaced as `meta_kv_node_cache_evictions` in K6a/K7.
pub static META_KV_NODE_CACHE_EVICTIONS: AtomicU64 = AtomicU64::new(0);

/// Node splits executed by the serialized SMO task (design §4.6/§10).
/// Surfaced as `meta_kv_node_splits` in K6a/K7.
pub static META_KV_NODE_SPLITS: AtomicU64 = AtomicU64::new(0);

/// Node compactions (1:1 CoW rewrites folding the bset log) executed by
/// the serialized SMO task (design §4.6/§10; compaction:append ratio
/// > ~1:8 ⇒ node_size or cadence mistuned). Surfaced as
/// `meta_kv_node_compactions` in K6a/K7.
pub static META_KV_NODE_COMPACTIONS: AtomicU64 = AtomicU64::new(0);

/// Commit-path revalidation retries (design §4.6: a writer locked a leaf
/// an SMO had superseded between resolution and lock — unlock, re-resolve,
/// retry; SMOs are rare and serialized, so the loop is short). Surfaced as
/// `meta_kv_commit_smo_retries` in K6a/K7.
pub static META_KV_COMMIT_SMO_RETRIES: AtomicU64 = AtomicU64::new(0);

/// Journal entry bytes physically written to ring pages (`res.len` per
/// committed entry — headers included). One half of the §8 row 7
/// write-amplification accounting; surfaced as `meta_kv_journal_bytes`
/// (design §10) on the stats inode in PR K7.
pub static META_KV_JOURNAL_BYTES: AtomicU64 = AtomicU64::new(0);

/// Journal entries written (one whole transaction each, §4.1). Surfaced
/// as `meta_kv_journal_entries` (design §10) in PR K7.
pub static META_KV_JOURNAL_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// Bset frames appended to node tails by writeback (§4.6 pt 1). Surfaced
/// as `meta_kv_node_appends` (design §10) in PR K7.
pub static META_KV_NODE_APPENDS: AtomicU64 = AtomicU64::new(0);

/// Bytes of those appended frames (4 KiB-padded) — the second half of the
/// §8 row 7 "node writeback counters" accounting. Surfaced as
/// `meta_kv_node_append_bytes` in PR K7.
pub static META_KV_NODE_APPEND_BYTES: AtomicU64 = AtomicU64::new(0);

/// Bytes of whole-node CoW rewrites (compaction / split successors —
/// header page + base bset; §4.6). Completes the §8 row 7 device-byte
/// accounting. Surfaced as `meta_kv_node_rewrite_bytes` in PR K7.
pub static META_KV_NODE_REWRITE_BYTES: AtomicU64 = AtomicU64::new(0);

/// §4.6 pt 2 checkpoint cycles completed (ledger records written).
/// Surfaced as `meta_kv_checkpoints` in PR K7.
pub static META_KV_CHECKPOINTS: AtomicU64 = AtomicU64::new(0);

/// PR M6 (design-metadata-throughput §5.4 D4): kernel post-op ctime
/// writeback echoes (`fuse_update_ctime` → `fuse_flush_times` →
/// times-only `FUSE_SETATTR`) **absorbed with zero journal entries** —
/// the refinement parks in the per-volume pending-times map and rides a
/// batched drain instead of a per-op commit. The G4 gate's mechanism
/// counter: rename/unlink entries/op ≤ 1.02/1.05 requires this to track
/// `meta_updates` for the storm shapes. Surfaced as
/// `meta_kv_times_echo_absorbed`.
pub static META_KV_TIMES_ECHO_ABSORBED: AtomicU64 = AtomicU64::new(0);

/// Pending-times drain transactions committed (ONE journal entry each,
/// carrying every pending refinement that still advances its inode).
/// The honest residual G4 pays for the echo: ≈ drain cadence, amortized
/// across every absorbed echo in the window — and the named counter the
/// M7 conveyor inherits if it ever folds drains into user batches.
/// Surfaced as `meta_kv_times_echo_drain_commits`.
pub static META_KV_TIMES_ECHO_DRAIN_COMMITS: AtomicU64 = AtomicU64::new(0);

/// Pending ctime/mtime refinements made durable by drain transactions
/// (records staged, not entries — pairs with
/// `META_KV_TIMES_ECHO_DRAIN_COMMITS` for the amortization ratio).
/// Surfaced as `meta_kv_times_echo_drained`.
pub static META_KV_TIMES_ECHO_DRAINED: AtomicU64 = AtomicU64::new(0);

/// Successful `commit_tx` executions per **construction site** of the
/// committed [`backend::KvMetaBackend`] transaction — the
/// metadata-throughput program's D4.a "debug hook counting `commit_tx`
/// call sites" (design-metadata-throughput §5.4). Each `KvTx` captures its
/// `#[track_caller]` construction [`std::panic::Location`]; a successful
/// journal-entry write counts one against that site, so per-op journal
/// entry ratios (`meta_kv_journal_entries`) decompose into *named*
/// committers (e.g. `backend.rs:<rename tx>` vs `backend.rs:<setattr tx>`).
/// Counts successful user commits only — SMO/compensation entries and the
/// checkpoint ledger are not `commit_tx` and are visible as the ambient
/// entries term instead. Surfaced on the stats inode as
/// `meta_kv_commit_sites`; read directly by
/// `tests/meta_entry_economy_tests.rs` (the OQ-1 pin).
pub static META_KV_COMMIT_SITES: once_cell::sync::Lazy<
    scc::HashMap<&'static std::panic::Location<'static>, AtomicU64>,
> = once_cell::sync::Lazy::new(scc::HashMap::new);

/// Count one successful `commit_tx` against `site` (lock-free after the
/// first commit from a site: an scc bucket read + one relaxed `fetch_add`).
pub fn note_commit_site(site: &'static std::panic::Location<'static>) {
    use std::sync::atomic::Ordering;
    if META_KV_COMMIT_SITES
        .read_sync(&site, |_, v| {
            v.fetch_add(1, Ordering::Relaxed);
        })
        .is_none()
    {
        match META_KV_COMMIT_SITES.entry_sync(site) {
            scc::hash_map::Entry::Occupied(o) => {
                o.get().fetch_add(1, Ordering::Relaxed);
            }
            scc::hash_map::Entry::Vacant(v) => {
                v.insert_entry(AtomicU64::new(1));
            }
        }
    }
}

/// Snapshot of the commit-site attribution map as `("file:line", count)`
/// rows, sorted by descending count (stats-inode serialization + the
/// entry-economy pin tests' calibration reads).
pub fn commit_sites_snapshot() -> Vec<(String, u64)> {
    use std::sync::atomic::Ordering;
    let mut out = Vec::new();
    META_KV_COMMIT_SITES.iter_sync(|k, v| {
        out.push((
            format!("{}:{}", k.file(), k.line()),
            v.load(Ordering::Relaxed),
        ));
        true
    });
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Errors from the pure KV encoding / fold layer.
///
/// Kept separate from [`crate::error::SqueezefsError`] so the contracts stay
/// typed for the K2+ layers (node, journal, backend) that map them onto the
/// crate error surface.
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    /// A 257th same-hash name: every `coll_seq` 0..=255 in the dentry chain
    /// is occupied (design §4.2). A clean insert rejection — never a panic —
    /// counted in [`META_KV_DENTRY_COLLISION_OVERFLOWS`].
    #[error("dentry hash-collision chain full (coll_seq 0..=255 all occupied); insert rejected")]
    DentryChainOverflow,

    /// A bset's stored xxh3 checksum does not match the bytes (design §4.3).
    #[error("bset checksum mismatch: stored {stored:#018x}, computed {computed:#018x}")]
    ChecksumMismatch { stored: u64, computed: u64 },

    /// Structurally invalid or truncated encoding: bad magic/version, a
    /// length field exceeding its container (design §9 bounds rule), record
    /// ordering violations, unknown record kinds or delta mask bits.
    #[error("corrupt KV encoding: {0}")]
    Corrupt(String),

    /// A readdir cookie whose biased payload exceeds the 62-bit dentry key
    /// suffix space — this filesystem never issued it (design §5.1).
    #[error("invalid readdir cookie {0:#x}: payload exceeds the 62-bit key-suffix space")]
    InvalidReaddirCookie(u64),

    /// A dentry or xattr name longer than the 255-byte `name_len: u8` record
    /// limit (design §4.2 value layouts).
    #[error("name length {len} exceeds the 255-byte record limit")]
    NameTooLong { len: usize },

    /// A record value larger than the per-volume cap
    /// `min(65,536, node_size/4)` (design §4.2) — enforced at the node layer
    /// on every write path (PR K2). A clean op rejection, never a panic.
    #[error(
        "record value length {len} exceeds the per-volume cap {cap} (min(65536, node_size/4))"
    )]
    ValueTooLarge { len: usize, cap: usize },

    /// A bset append that does not fit the node's unwritten tail — the
    /// signal for the caller (the K5/K6b writeback task) to compact
    /// (design §4.6 pt 1: "on-disk log area full ⇒ compact").
    #[error("node log area full: append needs {needed} bytes, tail has {available}")]
    NodeFull { needed: usize, available: usize },

    /// The §4.5 positional torn-tail classifier's **loud** outcome: a valid
    /// same-incarnation bset beyond the tear whose `journal_seq_horizon` is
    /// ≤ the durable journal tail. Checkpoint-covered data was barriered
    /// before its ledger record (§4.6 ordering), so a durable-required bset
    /// cannot legitimately follow a torn one — this is real corruption, not
    /// a power-loss artifact, and the node must fail loud.
    #[error(
        "corrupt node at {node_addr:#x}: checkpoint-covered bset at node offset {bset_offset} \
         (journal_seq_horizon {horizon} ≤ durable tail {durable_tail}) follows a torn bset"
    )]
    CheckpointCoveredBsetAfterTear {
        node_addr: u64,
        bset_offset: usize,
        horizon: u64,
        durable_tail: u64,
    },

    /// A journal entry larger than the 128 KiB whole-entry cap (design
    /// §4.1) — the writer-side guard at commit (PR K3); replay
    /// independently drops any `len` implying more without ever
    /// dereferencing it. A clean commit rejection, never a panic.
    #[error("journal entry length {len} exceeds the {cap}-byte whole-entry cap")]
    EntryTooLarge { len: u64, cap: u64 },

    /// §4.7 ENOSPC: a **user-op** extent allocation refused because
    /// granting it would dip the free budget to-or-below the compaction
    /// reserve — which stays intact so compaction/checkpoint internals can
    /// always fold appends and free space (no write-to-free-space
    /// deadlock). K6a/K6b map this onto the crate error path exactly like
    /// today's "Inode table full" → `ENOSPC` analog (`alloc.rs`). From
    /// `claim_internal` it means the heap is genuinely exhausted.
    #[error(
        "no space: {free} free extents with the {reserve}-extent compaction reserve \
         intact — allocation refused (ENOSPC)"
    )]
    NoSpace { free: u64, reserve: u64 },

    /// The checkpoint-task ring reserve could not admit an SMO's records
    /// right now (§4.4 pt 5): the caller (the per-volume checkpoint task
    /// — the only SMO driver) must run a **minimal drain** (barrier +
    /// ledger + `reusable_upto` advance, zero ring bytes — the §4.4 pt 5
    /// progress theorem) and retry. Never a panic, never a park: parking
    /// the one task that frees ring space is the R10 self-deadlock.
    #[error(
        "journal reserve exhausted: {needed} bytes of SMO records refused admission; \
         run a minimal checkpoint drain and retry"
    )]
    JournalReserveExhausted { needed: u64 },

    /// The pending-free list hit its cap (design §4.7 "capped"): the
    /// caller must force a checkpoint (whose durable barrier drains the
    /// list via `advance_durable`) — never reuse an extent whose retiring
    /// checkpoint is not yet durable. A clean rejection, never a panic.
    #[error(
        "pending-free list full ({pending} extents awaiting durable checkpoints); \
         checkpoint required before further frees"
    )]
    PendingFreeFull { pending: u64 },

    /// KV extent I/O failed (the `crate::uring_fs` paths — io_uring-only
    /// per AGENTS.md; a poisoned/torn device surfaces here as `EIO`).
    #[error("kv extent I/O failed: {0}")]
    Io(#[from] crate::error::SqueezefsError),

    /// D0 single-writer mount guard refusal
    /// (design-metadata-throughput §5.0): another writer holds the volume
    /// — flock held, fresh `writer_claim`, or a conflicting NVMe
    /// reservation. The message names the holder and the remedy; the
    /// mount exits nonzero.
    #[error("{0}")]
    Busy(String),
}

/// Map KV-layer errors onto the crate error surface (mount / CLI / trait
/// callers): device I/O passes through untouched; every other variant is
/// a typed-refusal-turned-loud-message, the `InvalidOperation` class the
/// v2 mount gates use.
impl From<KvError> for crate::error::SqueezefsError {
    fn from(e: KvError) -> Self {
        match e {
            KvError::Io(inner) => inner,
            other => {
                crate::error::SqueezefsError::InvalidOperation(format!("kv metadata: {other}"))
            }
        }
    }
}
