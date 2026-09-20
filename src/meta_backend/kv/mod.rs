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
pub mod alloc_lease;
pub mod appender;
pub mod backend;
pub mod block_map;
pub mod block_refs;
pub mod bset;
pub mod builder;
pub mod checkpoint;
pub mod conveyor_core;
pub mod epoch_core;
pub mod forest;
pub mod indirect_map;
pub mod ino_lane;
pub mod journal;
pub mod journal_core;
pub mod journal_lane;
pub mod node;
pub mod node_cache;
pub mod node_seq;
pub mod node_state_core;
pub mod record;
pub mod revalidate;
/// PR 7 — the shared-block index + refcount probes (`backend`'s child,
/// re-exported at the design's path).
pub use backend::shared_refs;
pub mod slot_cursor_core;
pub mod slot_lease;
pub mod slot_set;
pub mod slot_state;
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

// -- Reader-side node-cache revalidation (spec §6.8 item 2) ---------------
//
// All six are 0 for the whole life of a write mount — nothing arms
// revalidation there — so nonzero values are themselves the statement
// "this mount is a coherent reader".

/// Ledger polls a reader performed (one 128 KiB A/B root-ledger read
/// each). `polls` growing with `meta_kv_revalidate_epochs` flat is the
/// designed idle-writer steady state: a checkpoint cycle only writes a
/// record when it had work (`checkpoint.rs::tick`), so the cadence is
/// inert until the writer commits something.
pub static META_KV_REVALIDATE_POLLS: AtomicU64 = AtomicU64::new(0);

/// Epoch advances (polls that found a newer ledger record). Each one is
/// exactly one staleness step of the documented consistency model.
pub static META_KV_REVALIDATE_EPOCHS: AtomicU64 = AtomicU64::new(0);

/// Nodes dropped by revalidation drop passes — the reader's reload bill.
/// `nodes_dropped / epochs` is the live working-set size; if it approaches
/// the whole cache every poll, the operator's cadence is finer than the
/// workload wants (see docs/operations.md § Read-only coherent mounts).
pub static META_KV_REVALIDATE_NODES_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Hit-path rejections of a stale-stamped node (the lazy half of the drop
/// pass: a node published under a superseded epoch is a miss, even before
/// the sweep reaches it). Nonzero is normal under a busy writer.
pub static META_KV_REVALIDATE_STALE_SERVES: AtomicU64 = AtomicU64::new(0);

/// **Must stay 0**: dirty nodes a drop pass refused to drop. A reader has
/// no dirty nodes by construction, so any growth means revalidation was
/// armed on a mount that writes — the pass keeps the node (dropping it
/// would lose RAM records no disk image holds) and this counter is the
/// evidence.
pub static META_KV_REVALIDATE_DIRTY_SKIPS: AtomicU64 = AtomicU64::new(0);

/// Block keys an epoch advance purged through the R-6 unified purge
/// (`TieredCache::purge_block_key`) — the §6.8 item-5 remote trigger's
/// engagement gauge. 0 with `epochs` growing means the data-plane half is
/// not wired (honest, and visible, rather than silently absent).
pub static META_KV_REVALIDATE_KEYS_PURGED: AtomicU64 = AtomicU64::new(0);

/// Ledger bytes the reader's poll read (PR 5's predicted-slot-first read,
/// `revalidate::read_newest_ledger_from`): one 4 KiB slot per idle poll,
/// `k + 1` slots after `k` writer checkpoints, the whole 128 KiB ledger
/// only on the fallback below — `bytes ÷ polls` is the poll's cost.
pub static META_KV_REVALIDATE_LEDGER_READ_BYTES: AtomicU64 = AtomicU64::new(0);

/// Polls that fell back to the whole-ledger read: the predicted slot was
/// TORN (a writer mid-write, or damage) — the full read is what finds the
/// newest valid record in any slot — or the volume is partitioned (bit 8,
/// non-solo), where the round-robin prediction does not hold. Steady
/// growth on a solo volume is the torn-slot class.
pub static META_KV_REVALIDATE_LEDGER_FULL_READS: AtomicU64 = AtomicU64::new(0);

/// Extent loads a reader retried because the read raced the writer's
/// in-flight append (a torn frame followed by a complete one — §4.5's loud
/// verdict on a crashed writer, a plain race here). Bounded per load;
/// growth is normal on a reader of a hot volume, and a load that fails
/// past the budget still surfaces the verdict.
pub static META_KV_READER_LOAD_RETRIES: AtomicU64 = AtomicU64::new(0);

/// **Must stay 0**: partitioning violations refused at the node layer
/// (spec §6.2 closing / §6.3) — a non-authority appender attempting to
/// mutate interior state, an armed reader attempting to mutate anything,
/// or an append whose destination page already holds a peer's frame.
/// Every one of these is silent divergence prevented.
pub static META_KV_NODE_PARTITION_REFUSALS: AtomicU64 = AtomicU64::new(0);

/// Node splits executed by the serialized SMO task (design §4.6/§10).
/// Surfaced as `meta_kv_node_splits` in K6a/K7.
pub static META_KV_NODE_SPLITS: AtomicU64 = AtomicU64::new(0);

/// Node compactions (1:1 CoW rewrites folding the bset log) executed by
/// the serialized SMO task (design §4.6/§10; compaction:append ratio
/// > ~1:8 ⇒ node_size or cadence mistuned). Surfaced as
/// `meta_kv_node_compactions` in K6a/K7.
pub static META_KV_NODE_COMPACTIONS: AtomicU64 = AtomicU64::new(0);

/// Sibling merges executed by the serialized SMO task (design §4.6a —
/// two adjacent underfull nodes under one parent folded into ONE
/// successor; leaf and interior alike). The engagement gauge of the
/// leaf-merge SMO: it moves on any delete-heavy workload and stays 0 on a
/// fill-only one. Surfaced as `meta_kv_node_merges`.
pub static META_KV_NODE_MERGES: AtomicU64 = AtomicU64::new(0);

/// The level-≥ 1 subset of [`META_KV_NODE_MERGES`] (design §4.6a (c),
/// interior recursion): two adjacent underfull INTERIOR nodes folded into
/// one — the face that proves cross-parent shrinkage (underfull leaves
/// under different parents merge only after their parents did). Surfaced
/// as `meta_kv_interior_merges`.
pub static META_KV_INTERIOR_MERGES: AtomicU64 = AtomicU64::new(0);

/// Root collapses (design §4.6a (c)): a root left with exactly one live
/// child made that child the root — the ONLY way the tree's height
/// decreases. A root-swap SMO (no pointer record; dying floor at the
/// entry start). Surfaced as `meta_kv_root_collapses`.
pub static META_KV_ROOT_COLLAPSES: AtomicU64 = AtomicU64::new(0);

/// **Slot-tree forest** (design-symmetric-metadata §5.2, bit 17): guest
/// slot trees minted on a slot's first record (one extent each — the
/// engagement gauge of lazy minting; 0 for the life of every un-stamped
/// mount). Surfaced as `meta_kv_forest_slot_trees_minted`.
pub static META_KV_FOREST_SLOT_TREES_MINTED: AtomicU64 = AtomicU64::new(0);

/// Slot-tree roots the checkpoint published into tree 0 as `slot_state`
/// records (one per guest slot whose root moved since its last
/// publication). `publishes ≤ checkpoints × slot trees`; 0 on un-stamped
/// mounts. Surfaced as `meta_kv_forest_root_publishes`.
pub static META_KV_FOREST_ROOT_PUBLISHES: AtomicU64 = AtomicU64::new(0);

/// **Must-stay-0 tripwire**: a record that reached a forest volume's
/// commit or replay path carrying a key the §5.2.1 codec refuses (a kind
/// byte that is another tree's id, a wrong length, a by-block prefix
/// other than refs) — the partition-violation class of KD-SYM-2's "a
/// kind byte is never another tree's id". Surfaced as
/// `meta_kv_forest_key_violations`.
pub static META_KV_FOREST_KEY_VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// Window records a NON-WRITER open (reader, co-writer, probe) of a forest
/// volume SKIPPED at replay because their slot tree had no published root
/// yet (tree 0 did not name it — the writer minted the slot after its
/// last checkpoint). A mount that may not write mints nothing: those
/// records are served after the writer's next publication, at the
/// reader's next poll — the S5 bounded-stale contract. 0 for the life of
/// every write mount by construction. Surfaced as
/// `meta_kv_forest_reader_window_skips`.
pub static META_KV_FOREST_READER_WINDOW_SKIPS: AtomicU64 = AtomicU64::new(0);

/// Directory entries a NON-WRITER open of a forest volume withheld from a
/// `readdir` page because the CHILD's slot tree had no published root yet
/// (the parent's leaf lists the name — a dentry lives in the parent's
/// slot; the child's records live in the child's, which this mount does
/// not hold). Withholding the name is what keeps the partial view a
/// consistent snapshot: `readdir` never lists a name `lookup` refuses, and
/// both serve the child at the poll that names its slot. 0 for the life
/// of every write mount by construction. Surfaced as
/// `meta_kv_forest_reader_unpublished_children`.
pub static META_KV_FOREST_READER_UNPUBLISHED_CHILDREN: AtomicU64 = AtomicU64::new(0);

/// **Must-stay-0 tripwire** (design-symmetric-metadata §5.3.4, PR 2):
/// one `(kind, key)` found in TWO appender rings' replay windows —
/// KD-SYM-4's one-ring-per-key invariant broken; the mount refuses.
pub static META_KV_REPLAY_KEY_VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// **Must-stay-0 tripwire**: a ring carried a record for a slot tree its
/// appender did not lease (or the manager's structure in a content
/// appender's ring); the mount refuses.
pub static META_KV_REPLAY_LEASE_VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// **Must-stay-0 tripwire**: an allocator delta in a ring whose appender
/// holds no grant covering the extent; the mount refuses.
pub static META_KV_REPLAY_EXTENT_VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// **Must-stay-0 tripwire** (design-symmetric-metadata §5.4.1, PR 4): a
/// leaf mutation of a slot tree this mount does NOT lease — or one
/// mid-handover — refused at `CachedNode::apply_locked` on an ARMED
/// symmetric mount; the writer twin of `meta_kv_node_partition_refusals`.
/// Surfaced as `meta_kv_leaf_lease_refusals`.
pub static META_KV_LEAF_LEASE_REFUSALS: AtomicU64 = AtomicU64::new(0);

/// Bset frames the §5.8.2 screen classified FOREIGN at a load on a
/// symmetric-forest volume (design-symmetric-metadata, PR 5): a frame
/// past its slot's recorded tail under an older lease generation (rule
/// 2), a frame whose generation is lower than an earlier frame's in the
/// same log (rule 3), or — on a non-PR substrate — a frame above the
/// current generation (rule 1). Every one is a zombie's append the walk
/// cut the log at; 0 on every PR substrate by construction, 0 on every
/// flat volume (v1 frames carry no stamp). Surfaced as
/// `foreign_frames_screened`.
pub static META_KV_FOREIGN_FRAMES_SCREENED: AtomicU64 = AtomicU64::new(0);

/// **Must-stay-0 tripwire** (§5.8.2 rule 1 on a DEVICE-FENCED substrate):
/// a frame stamped with a slot generation ABOVE the current lease
/// generation — a write the reservation should have rejected. Surfaced as
/// `appender_fence_breach`.
pub static META_KV_APPENDER_FENCE_BREACH: AtomicU64 = AtomicU64::new(0);

/// **Must-stay-0 on PR**: the after-the-fact face of §5.8.2's residual
/// class (ii) — a foreign frame found at a position this lessee had
/// already written (the destination-page probe at an append, or a
/// screened frame with this lessee's own frames behind it at a load):
/// a zombie overwrote an acked frame on a non-PR substrate. The load
/// refuses loud; the append refuses loud. Surfaced as
/// `foreign_frame_overwrite_detected`.
pub static META_KV_FOREIGN_FRAME_OVERWRITE_DETECTED: AtomicU64 = AtomicU64::new(0);

/// DUR-4 law 2 engagements: bitmap page images whose requested generation
/// tied or trailed the page's newest on-disk copy and were RAISED above it
/// (`alloc_ext::write_dirty_pages`). The shape it was written for is a
/// checkpoint RETRY after a failed cycle — on a healthy mount, across
/// clean unmount/remount cycles included, it stays 0 (review round 2,
/// Issue 18: the mount resumes its checkpoint seq above the bitmap's
/// newest generation, not the ledger's alone).
pub static META_KV_BITMAP_GENERATION_RAISES: AtomicU64 = AtomicU64::new(0);

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

/// Freeze-time shadow-fold drops (perf/meta-plane-writes, 2026-07-30):
/// overlay records excluded from frozen bsets because a newer same-key
/// `Put`/`Delete` in the same freeze completely shadows them (fold
/// algebra §4.2) — pure device-byte savings, the engagement gauge for
/// the node-writeback half of the per-block meta write economy. High
/// rates are the DESIGNED steady state under same-key commit storms
/// (block publishes, claim heartbeats); 0 on such a workload means the
/// fold regressed. Surfaced as `meta_kv_node_freeze_shadow_dropped`.
pub static META_KV_NODE_FREEZE_SHADOW_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Checkpoint flush steps that appended a PRE-EXISTING frozen delta (an
/// SMO froze the node for its fold and failed before its swap) while the
/// open delta held newer records — the floor of those records KEPT on
/// the node (PR 13: taking it made them invisible to every later flush
/// and the tail; a joined appender's clean leave released a tree whose
/// RAM fold said `Tombstone` and whose image said `Live`). Engagement of
/// the fix; 0 on a solo mount whose SMOs never fail mid-way. Surfaced as
/// `meta_kv_flush_floor_kept`.
pub static META_KV_FLUSH_FLOOR_KEPT: AtomicU64 = AtomicU64::new(0);

/// Traversals of a PROJECTION tree (tree 0 / the manager's native tree on
/// a JOINED appender) whose root pointer named a recycled image — the
/// manager compacted the tree and its old root's extent was re-granted —
/// that re-adopted the manager's newest root through the installed
/// projection refresh instead of exhausting the restart budget (PR 13,
/// the `sym-scale` N = 8 row: `restarts [root-seq] = 256`, EIO on the
/// user op). Surfaced as `meta_kv_projection_root_refreshes`; 0 on the
/// manager and every flat mount.
pub static META_KV_PROJECTION_ROOT_REFRESHES: AtomicU64 = AtomicU64::new(0);

/// Ledger records written by a checkpoint-class durable step that
/// CONSUMED a checkpoint seq outside a cycle — PR 2's ring growth, the
/// appender leave, PR 10's region release (`KvMetaBackend::
/// consume_checkpoint_seq_for_bitmap`): each restates the last cycle's
/// word at the consumed seq so the ledger stays DENSE and PR 5's
/// predicted-slot poll never stops on a gap (PR 13, defect 25).
/// Surfaced as `meta_kv_ledger_restatements`; 0 on every flat mount.
pub static META_KV_LEDGER_RESTATEMENTS: AtomicU64 = AtomicU64::new(0);

/// A token reader's poll that read the WHOLE ledger after a ring's worth
/// of polls stopped on an older predicted slot — the gap belt
/// (`KvMetaBackend::read_root_epoch`, PR 13 defect 25). ≈ 0 against a
/// writer that restates every consumed seq; surfaced as
/// `meta_kv_revalidate_gap_scans`.
pub static META_KV_REVALIDATE_GAP_SCANS: AtomicU64 = AtomicU64::new(0);

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

/// Option A — pending-free coverage (design-smo-replay-currency §2-A,
/// PR 4): retirements that entered the coverage-gated pending FIFO —
/// live SMO frees plus mount-replayed frees re-parked in-window. With
/// [`META_KV_PENDING_FREE_RELEASED`] this pairs into the backlog gauge
/// (`parked − released ≈` the per-volume `meta_kv_pending_free` sum);
/// steady-state backlog under the measured 939-SMO/s storm is ≈ 1.4 % of
/// the 65,536 cap. Surfaced as `meta_kv_pending_free_parked`.
pub static META_KV_PENDING_FREE_PARKED: AtomicU64 = AtomicU64::new(0);

/// Option A (§2-A): parked retirements released back to the claimable
/// pool — only ever by a durable tail passing their free-record seq
/// (`advance_durable` post-barrier). A `parked` that outruns `released`
/// on a quiet mount means the coverage tail wedged (the at-cap protocol's
/// forced cycles + bounded-loud terminal exist for exactly that).
/// Surfaced as `meta_kv_pending_free_released`.
pub static META_KV_PENDING_FREE_RELEASED: AtomicU64 = AtomicU64::new(0);

/// The §4.7 cycle-break's engagement gauge (P2 2026-07-26 §9 fix
/// direction a): retirements FORCED past the at-cap FIFO into the
/// unbounded overflow — the checkpoint flush pass's own compactions
/// (whose refusal was the closed pinned-floor dependency cycle) plus
/// mount-side re-parking of beyond-cap replayed windows. Steady state is
/// 0 (the FIFO absorbs everything below cap); sustained growth means the
/// volume lives at the cap — durable-tail coverage is lagging SMO
/// pressure. Surfaced as `meta_kv_pending_free_overflow`.
pub static META_KV_PENDING_FREE_OVERFLOW: AtomicU64 = AtomicU64::new(0);

/// Replayed in-window `free` records DROPPED at mount because their
/// extent is a LIVE tree root — the root-swap carve-out's retirement
/// (design-smo-replay-currency §2 C′: a root swap journals no pointer
/// record, so a kill before the record naming its successor leaves the
/// mount replaying through the predecessor, which is live again;
/// parking its free released the live root at the first post-mount
/// checkpoint). One per unpublished root swap in the recovered window
/// (flat rings and appender rings alike); 0 on every clean remount.
/// Surfaced as `meta_kv_replay_root_frees_dropped`.
pub static META_KV_REPLAY_ROOT_FREES_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Pre-RC spec §6.2 item 1 (incompat bit 8): durable block-reference
/// records **staged** into layout transactions — one `Put` per reference
/// taken. Rides the publish tx, so `staged/publish` is the accounting's
/// per-op cost and a flat counter on a striped write storm means the
/// wiring regressed to derived-only. Surfaced as
/// `meta_kv_block_refs_staged`.
pub static META_KV_BLOCK_REFS_STAGED: AtomicU64 = AtomicU64::new(0);

/// §6.2 item 1: durable block-reference records **released** (one
/// `Delete` per reference dropped — displacement, truncate/punch prune,
/// unlink destroy, clone teardown). `staged − released` tracks the live
/// durable population; a released count that never moves under overwrite
/// churn means displaced blocks are leaking their accounting.
/// Surfaced as `meta_kv_block_refs_released`.
pub static META_KV_BLOCK_REFS_RELEASED: AtomicU64 = AtomicU64::new(0);

/// §6.2 item 1: references seeded into the RAM allocator **from durable
/// records** at mount — the count that replaces the inode-tree walk.
/// Nonzero on a bit-8 volume with data; structurally 0 on an un-stamped
/// volume (which still walks). Surfaced as
/// `meta_kv_block_refs_recovered`.
pub static META_KV_BLOCK_REFS_RECOVERED: AtomicU64 = AtomicU64::new(0);

/// §6.2 item 1: **durable-vs-derived drift** — blocks whose durable
/// reference population disagreed with the layout walk's census. The
/// oracle's verdict, and a **must-stay-0 tripwire**: any nonzero value is
/// an fsck finding (`FindingId::C8DurableRefDrift`) and means the durable
/// ledger and the layouts that justify it diverged. Surfaced as
/// `meta_kv_block_refs_drift`.
pub static META_KV_BLOCK_REFS_DRIFT: AtomicU64 = AtomicU64::new(0);

/// §6.2 item 1: accounting operations **dropped because no resolver /
/// no engaged tree** was available (offline tools and unit fixtures that
/// build a router without a bit-8 meta backend). Honest instead of
/// silent: a mount that grows this is running derived-only accounting
/// while believing otherwise. Surfaced as
/// `meta_kv_block_refs_unresolved`.
pub static META_KV_BLOCK_REFS_UNRESOLVED: AtomicU64 = AtomicU64::new(0);

/// PB-class files, PR 1 (docs/design-kvmap-block-map-tree.md): block-map
/// records **staged** as `Put`s into layout transactions — one per
/// mapping bound. Rides the publish tx (design §3: one conveyor pass =
/// one journal entry), so a flat counter under a crossing/publish storm
/// means the wiring regressed to the blob path. Surfaced as
/// `meta_kv_block_map_puts` (PR 2).
pub static META_KV_BLOCK_MAP_PUTS: AtomicU64 = AtomicU64::new(0);

/// PB-class files, PR 1: block-map records **staged** as `Delete`s (one
/// per mapping removed — displacement, the A1 residue sweep, the unlink
/// sweep's chunks). Surfaced as `meta_kv_block_map_deletes` (PR 2).
pub static META_KV_BLOCK_MAP_DELETES: AtomicU64 = AtomicU64::new(0);

/// PB-class files, PR 3 (design §8 #5 — the PR-1 merged lookups counter
/// SPLIT, or §5's gauges are un-derivable): `get_block_mapping` exact
/// point resolutions. Surfaced as `meta_kv_block_map_lookup_exact`.
pub static META_KV_BLOCK_MAP_LOOKUP_EXACT: AtomicU64 = AtomicU64::new(0);

/// PB-class files, PR 6a (design §12/A6): exact-miss lookups that ran the
/// bounded run-floor probe (whether or not a covering run answered) — the
/// point-over-run read law's engagement face. Surfaced as
/// `meta_kv_block_map_lookup_floor`.
pub static META_KV_BLOCK_MAP_LOOKUP_FLOOR: AtomicU64 = AtomicU64::new(0);

/// PB-class files, PR 6a (design §5 `block_map_tree_run_puts`): RUN/RUN2
/// records **staged** as `Put`s — the run-emission engagement gauge
/// (blocks÷records collapse is `map_migrate_records` vs the covered
/// span). Surfaced as `meta_kv_block_map_run_puts`.
pub static META_KV_BLOCK_MAP_RUN_PUTS: AtomicU64 = AtomicU64::new(0);

/// PB-class files, PR 3 (§8 #5): `block_map_range` window reads — the
/// fetch-rehydration/walker face; ops ÷ entries is the live leaf
/// amortization. Surfaced as `meta_kv_block_map_lookup_range`.
/// (The §8 #5 overlay-hit arm has no counter yet BY CONSTRUCTION: PR 3's
/// read consumers resolve from the whole rehydrated RAM map — Rev 1.3
/// #2 — so no per-index overlay-vs-tree decision point exists to count;
/// it lands with the §8 window cache on the map-RAM-boundedness rung.)
pub static META_KV_BLOCK_MAP_LOOKUP_RANGE: AtomicU64 = AtomicU64::new(0);

/// Finding 46: tree-7 RECORDS returned by `block_map_range` pages — the
/// direct "tree reads per publish" instrument (a page count hides the
/// page width: the 2026-09-02 field row paid ~4.6 pages ≈ the ino's
/// WHOLE record population per steady-state publish). `records ÷
/// publishes` growing with file size is the whole-map diff running where
/// only a window should. Surfaced as `meta_kv_block_map_range_records`.
pub static META_KV_BLOCK_MAP_RANGE_RECORDS: AtomicU64 = AtomicU64::new(0);

/// PB-class files, PR 3 (§8 #5): tree-7 LEAF nodes demand-paged off the
/// device — counted at the node-cache miss site (`CachedNode` carries
/// `tree_id`, so attribution is one compare on the already-cold load
/// path). The §5 `block_map_tree_leaf_reads` gauge: cold-map
/// amplification ≈ `leaf_reads × node_size` per §8 #5. Surfaced as
/// `meta_kv_block_map_leaf_reads`.
pub static META_KV_BLOCK_MAP_LEAF_READS: AtomicU64 = AtomicU64::new(0);

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
/// across every absorbed echo in the window. **PR M7 disposition (the
/// M6 hand-off)**: drain commits are ordinary `commit_tx` transactions,
/// so they ride the conveyor like every user commit — when a drain's
/// arrival overlaps user traffic it co-batches into the same pass
/// (one lock window, one write, one barrier) with zero dedicated
/// machinery; this counter keeps counting drain *transactions* either
/// way (the fold shows up in `meta_commit_group_size`, not here).
/// Surfaced as `meta_kv_times_echo_drain_commits`.
pub static META_KV_TIMES_ECHO_DRAIN_COMMITS: AtomicU64 = AtomicU64::new(0);

/// Pending ctime/mtime refinements made durable by drain transactions
/// (records staged, not entries — pairs with
/// `META_KV_TIMES_ECHO_DRAIN_COMMITS` for the amortization ratio).
/// Surfaced as `meta_kv_times_echo_drained`.
pub static META_KV_TIMES_ECHO_DRAINED: AtomicU64 = AtomicU64::new(0);

/// Pending refinements DROPPED because their ino's forest slot is leased
/// by another appender at the drain (symmetric PR 13, defect 31): the
/// slot moved out from under a parked refinement — the ordinary path
/// drains a slot's refinements BEFORE its release (`transfer_slot_
/// locked`), so this is the belt (a stale echo through a recovered or
/// projected image; a slot recovered from a dead appender). A foreign
/// refinement cannot be committed here (the door refuses the whole
/// batch, and before this counter existed one such ino wedged the
/// volume's every drain — every fsync of the manager's own files
/// answered `EAGAIN` until the map was emptied by a remount). Surfaced
/// as `meta_kv_times_echo_foreign_dropped`; 0 on every unarmed mount.
pub static META_KV_TIMES_ECHO_FOREIGN_DROPPED: AtomicU64 = AtomicU64::new(0);

/// PR M9 (design-metadata-throughput §5.7 D7.a): point lookups served
/// straight from the **fold-forward overlay head** — the materialized
/// folded value riding the newest open-delta record of the key, kept
/// current at apply time under the node write lock the committer already
/// holds. Zero record decodes on this path; the create storm's hot parent
/// inode probe lands here between freezes. Surfaced as
/// `meta_kv_fold_head_serves` (design §9).
pub static META_KV_FOLD_HEAD_SERVES: AtomicU64 = AtomicU64::new(0);

/// PR M9 (§5.7 D7.b): snapshot fold-memo hits — a bset-resident key's
/// fold served from the immutable snapshot's populate-once memo cells
/// (zero decodes; latch-free probe). The create-storm hit rate
/// (`hits / (hits + misses)`) is the acceptance number. Surfaced as
/// `meta_kv_fold_memo_hits` (design §9).
pub static META_KV_FOLD_MEMO_HITS: AtomicU64 = AtomicU64::new(0);

/// PR M9 (§5.7 D7.b): memo-eligible folds that ran from scratch (key not
/// in the snapshot's memo cells yet — first fold after a snapshot swap,
/// or the fixed [`node_cache::FOLD_MEMO_CAPACITY`] cells were exhausted).
/// Surfaced as `meta_kv_fold_memo_misses` (design §9).
pub static META_KV_FOLD_MEMO_MISSES: AtomicU64 = AtomicU64::new(0);

/// PR M9 (§5.7 D7 memory accounting): **gauge** — bytes currently held by
/// snapshot fold-memo cells across every live snapshot (keys + owned
/// folded values + fixed per-cell overhead). Charged/discharged exactly
/// (populate adds; the memo's `Drop` subtracts when its snapshot dies at
/// the next swap or eviction), and the same bytes ride the node-cache
/// budget (`SQUEEZEFS_META_NODE_CACHE_MB`) through `NodeCache::
/// cached_bytes`. The M9 tiny-budget storm gate reads this: eviction must
/// keep it bounded. Surfaced as `meta_kv_fold_memo_bytes` (design §9).
pub static META_KV_FOLD_MEMO_BYTES: AtomicU64 = AtomicU64::new(0);

/// PR M9 (§5.7): record decodes performed by fold execution — one count
/// per `InodeValue`/`InodeDelta` decode inside [`record::fold_newest_first`]
/// and [`record::fold_forward`]. **The D7 acceptance pin**: an overlay-head
/// or memo serve must move this by exactly zero (the RED-first
/// zero-decode contract in `tests/kv_fold_slimming_tests.rs`); the
/// baseline's 16 %-of-daemon-CPU re-decode tax is this counter's rate.
/// Deliberately not on the stats JSON (design §9 names the four
/// `meta_kv_fold_*` fields above; this is the tests' and profiler's pin).
pub static META_KV_FOLD_RECORD_DECODES: AtomicU64 = AtomicU64::new(0);

/// Write-commit-economy campaign (2026-07-30): block-publish commits
/// that staged a **layout delta record** (O(batch) journal bytes)
/// instead of re-serializing the whole layout — the lever-2 engagement
/// gauge (`layout_delta_commits` on the stats inode). A streaming write
/// workload whose publishes stopped moving this while
/// [`META_KV_LAYOUT_FULL_COMMITS`] grows means the eligibility ladder
/// regressed (every publish is paying O(block_map) journal bytes again).
pub static META_KV_LAYOUT_DELTA_COMMITS: AtomicU64 = AtomicU64::new(0);

/// Write-commit-economy campaign: layout publishes that took the
/// full-`Put` path (first-ever persist, indirect spill, truncate/punch
/// shapes, chain-cap re-bases, ratchet fallbacks). Healthy streaming
/// keeps this ≪ [`META_KV_LAYOUT_DELTA_COMMITS`].
pub static META_KV_LAYOUT_FULL_COMMITS: AtomicU64 = AtomicU64::new(0);

/// Journal-value bytes carried by layout delta records (the collapsed
/// O(batch) term; compare against `meta_kv_journal_bytes` growth).
pub static META_KV_LAYOUT_DELTA_BYTES: AtomicU64 = AtomicU64::new(0);

/// Layout delta records folded onto a base at read/compaction/replay
/// time (fold-side engagement; each count is one delta applied).
pub static META_KV_LAYOUT_DELTA_FOLDS: AtomicU64 = AtomicU64::new(0);

/// PR M7 (design-metadata-throughput §5.5 D5): transactions per conveyor
/// batch — the group-formation histogram the G3 gate reads
/// (`meta_commit_group_size`; strict-mode median ≥ 4 under the 8-writer
/// storm is a gate input, ≈ 1 means the conveyor regressed to the
/// baseline's measured failure mode). Buckets are **exact for 1–8** so a
/// median-≥4 verdict never rides bucket rounding, then power-of-two up
/// to the 64-tx cap.
pub static META_COMMIT_GROUP_SIZE: CommitGroupSizeHistogram = CommitGroupSizeHistogram::new();

/// Entry bytes drained per conveyor batch, summed (`meta_commit_group_bytes`
/// with [`META_CONVEYOR_LEADER_PASSES`] gives mean batch fill vs the
/// `SQUEEZEFS_META_COMMIT_BATCH_BYTES` cap).
pub static META_COMMIT_GROUP_BYTES: AtomicU64 = AtomicU64::new(0);

/// Conveyor batch passes executed (`meta_conveyor_leader_passes` — one per
/// drained batch; pairs with the group-size histogram total).
pub static META_CONVEYOR_LEADER_PASSES: AtomicU64 = AtomicU64::new(0);

/// Conveyor pass panics contained by the §5.5 panic guard
/// (`meta_conveyor_pass_panics`): > 0 means a batch failed LOUD — budget
/// released, reservation completed-as-abandoned, oneshots failed EIO,
/// journal-failure lattice bumped — never a silent `completed_upto` wedge.
pub static META_CONVEYOR_PASS_PANICS: AtomicU64 = AtomicU64::new(0);

/// Conveyor GROUP commits (`meta_conveyor_group_commits` — D-1c, e2e perf
/// audit §5.3 row 1): sets of staged transactions enqueued under ONE
/// queue-lock acquisition (`KvMetaBackend::commit_tx_group`), so a drain
/// can never take part of one — the owner-side mechanism behind "one
/// shipped frame = one apply pass". Counted once per non-empty group.
pub static META_CONVEYOR_GROUP_COMMITS: AtomicU64 = AtomicU64::new(0);

/// Member transactions those groups carried (`meta_conveyor_group_txs`);
/// `group_txs ÷ group_commits` is the live group size — ≈ the served
/// frame's independent-call width on a grouping authority, 0 on a solo
/// mount and under `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP=0` by construction.
pub static META_CONVEYOR_GROUP_TXS: AtomicU64 = AtomicU64::new(0);

/// Conveyor WINDOWS in flight (`meta_conveyor_windows_inflight`): batches
/// past their RAM apply whose journal write has been submitted but whose
/// members are not yet at a terminal outcome — i.e. the applied-but-
/// not-yet-acked population between the conveyor's apply stage and its
/// durability stage (e2e perf audit DLM #2, D-2). A serialized conveyor
/// (apply → write → ack in one server) can never read above 1; the
/// two-stage conveyor's engagement instrument is this gauge's high-water
/// mark ([`META_CONVEYOR_WINDOWS_INFLIGHT_HWM`]) reading ≥ 2 under load.
/// Bounded structurally by the ring's admissible capacity (§4.4 pt 5:
/// every window holds a registered reservation, and admission parks on
/// ring space before any node lock) — never by a constant.
pub static META_CONVEYOR_WINDOWS_INFLIGHT: AtomicU64 = AtomicU64::new(0);

/// High-water mark of [`META_CONVEYOR_WINDOWS_INFLIGHT`] since process
/// start (`meta_conveyor_windows_inflight_hwm`) — the D-2 engagement
/// instrument: 1 = the serialized shape, ≥ 2 = journal writes overlapped
/// the next batch's apply.
pub static META_CONVEYOR_WINDOWS_INFLIGHT_HWM: AtomicU64 = AtomicU64::new(0);

/// Durability-stage passes (`meta_conveyor_durability_passes`): one per
/// group of windows the durability lane took to a terminal outcome
/// together (in journal order; strict cadence = one coalesced barrier per
/// group). Windows ÷ passes is the lane's live coalesce factor; growing
/// [`META_CONVEYOR_WINDOWS_INFLIGHT`] while this stays flat is the
/// stalled-durability-lane signature.
pub static META_CONVEYOR_DURABILITY_PASSES: AtomicU64 = AtomicU64::new(0);

/// Note a window entering the in-flight population (apply stage handoff).
pub(crate) fn note_window_inflight() {
    let now = META_CONVEYOR_WINDOWS_INFLIGHT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    META_CONVEYOR_WINDOWS_INFLIGHT_HWM.fetch_max(now, std::sync::atomic::Ordering::Relaxed);
}

/// Note a window reaching its terminal outcome (every member answered).
pub(crate) fn note_window_done() {
    META_CONVEYOR_WINDOWS_INFLIGHT.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
}

/// Entries queued on ANY conveyor (tx + layout, every volume) and not
/// yet drained by a pass — the wedge census's stalled-conveyor gauge
/// (zc-bridge-cqe-wedge, 2026-08-07): growing while
/// [`META_CONVEYOR_LEADER_PASSES`] stays flat IS the stalled signature.
/// Maintained by `conveyor_core` (enqueue +1, drain −n) so no fan-out
/// path can leak it.
pub static META_CONVEYOR_QUEUED: AtomicU64 = AtomicU64::new(0);

/// Committers parked on their conveyor oneshot RIGHT NOW (the wedge
/// census's writers-behind-the-conveyor gauge): incremented before the
/// fan-out park, decremented on the answer — panic-safe by Drop guard.
pub static META_COMMIT_PARKED: AtomicU64 = AtomicU64::new(0);

/// The layout-merge (Lever B publish) twin of [`META_COMMIT_PARKED`].
pub static META_PUBLISH_PARKED: AtomicU64 = AtomicU64::new(0);

/// RAII decrement for the parked gauges (panic-safe: a committer future
/// dropped mid-park must not strand the census).
pub struct ParkedGaugeGuard(&'static AtomicU64);

impl ParkedGaugeGuard {
    pub fn enter(gauge: &'static AtomicU64) -> Self {
        gauge.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(gauge)
    }
}

impl Drop for ParkedGaugeGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The conveyor group-size histogram (design-metadata-throughput §9):
/// exact buckets 1–8 (the G3 median band), then ≤16 / ≤32 / ≤64 / >64.
pub struct CommitGroupSizeHistogram {
    buckets: [AtomicU64; 12],
}

/// Bucket labels for [`CommitGroupSizeHistogram`] (stats-JSON keys).
pub const COMMIT_GROUP_SIZE_LABELS: [&str; 12] = [
    "1", "2", "3", "4", "5", "6", "7", "8", "<=16", "<=32", "<=64", ">64",
];

impl CommitGroupSizeHistogram {
    #[allow(clippy::new_without_default)] // static-initializer const fn
    pub const fn new() -> Self {
        Self {
            buckets: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }

    fn bucket_index(n: usize) -> usize {
        match n {
            0 => 0, // empty batches are never recorded; clamp defensively
            1..=8 => n - 1,
            9..=16 => 8,
            17..=32 => 9,
            33..=64 => 10,
            _ => 11,
        }
    }

    /// Record one drained batch of `n` transactions.
    pub fn record(&self, n: usize) {
        use std::sync::atomic::Ordering;
        self.buckets[Self::bucket_index(n)].fetch_add(1, Ordering::Relaxed);
    }

    /// Bucket snapshot in [`COMMIT_GROUP_SIZE_LABELS`] order.
    pub fn snapshot(&self) -> [u64; 12] {
        use std::sync::atomic::Ordering;
        std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed))
    }

    /// Total batches recorded.
    pub fn total(&self) -> u64 {
        self.snapshot().iter().sum()
    }

    /// Conservative median group size: the **lower bound** of the bucket
    /// containing the middle sample (exact for sizes 1–8), so a G3
    /// "median ≥ 4" verdict can never be inflated by bucket rounding.
    /// `None` when nothing was recorded.
    pub fn median_lower_bound(&self) -> Option<u64> {
        const LOWER: [u64; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 17, 33, 65];
        let snap = self.snapshot();
        let total: u64 = snap.iter().sum();
        if total == 0 {
            return None;
        }
        let mid = total.div_ceil(2);
        let mut seen = 0u64;
        for (i, &c) in snap.iter().enumerate() {
            seen += c;
            if seen >= mid {
                return Some(LOWER[i]);
            }
        }
        None
    }
}

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

    /// A §4.6 freeze-swap refused by the node's lifecycle word
    /// ([`node_state_core::FreezeRefused`]) — a PROTOCOL state on the
    /// serialized writeback/SMO task (superseded / already-freezing /
    /// not-dirty), never an encoding corruption. Historically wrapped in
    /// [`Self::Corrupt`], whose "corrupt KV encoding" text sent the
    /// 2026-08-19 AlreadyFreezing-wedge field hunt after phantom device
    /// corruption.
    #[error("freeze refused on node {node_addr:#x}: {refusal:?}")]
    FreezeRefused {
        node_addr: u64,
        refusal: node_state_core::FreezeRefused,
    },

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

    /// An appender's SMO needs an image extent and its extent GRANT is
    /// exhausted (design-symmetric-metadata §5.3.3): the manager's refill
    /// has not answered — the appender's dependency on a live manager,
    /// bounded by `manager_dependency_stall_bound_ms`. The flush pass
    /// defers the node (counted `manager_dependency_stalls`); a user op
    /// that needs the extent refuses `EAGAIN`-class, never `ENOSPC` (the
    /// heap is not full — the appender is out of grant).
    /// `needed` = the images the refused act needs IN TOTAL (a split's
    /// parts + its root), so the reactive refill asks for exactly that —
    /// a refill sized to a constant re-answered the remainder verbatim
    /// for ever when one SMO needed more (PR 4 review round 3).
    #[error(
        "appender {appender}'s extent grant is exhausted ({unclaimed} unclaimed, {needed} \
         needed) and the manager has not refilled it — retry (EAGAIN)"
    )]
    GrantExhausted {
        appender: u32,
        unclaimed: u64,
        needed: u64,
    },

    /// A mutation of a forest slot another appender LEASES (design-
    /// symmetric-metadata §5.1.4, PR 4; review round 2 Issue 6): the
    /// commit door's "ship to the holder" answer — the ship is the
    /// metanode arm (PR 6/12), so until it lands the caller retries
    /// `EAGAIN`-class once the slot is this mount's or the ship exists.
    /// Never the corruption class: a foreign lease is a scheduling
    /// outcome, and a slot mid-handover PARKS at the door instead
    /// (`slot_door_parks`). `slot` is the forest slot.
    #[error(
        "forest slot {slot} is leased by appender {holder} (g {g}) — a mutation of a foreign \
         slot ships to its holder (design-symmetric-metadata §5.1.4, the metanode arm; \
         PR 6/12) — retry (EAGAIN)"
    )]
    SlotBusy { slot: u32, holder: u32, g: u32 },

    /// A rotor ask (`AcquireSlots { want }`) that would take `appender`
    /// past the `2 × M` rotor cap (§5.1.2) — the overflow arm's designed
    /// past-cap answer (the smallest rotor tree takes the mint), counted
    /// `slot_rotor_cap_refusals`, never `manager_verb_refusals`.
    #[error(
        "appender {appender} holds {held} rotor slot(s) and asks {want} more — the manager \
         grants rotor slots up to 2 × M = {cap} (SQUEEZEFS_SYM_MINT_SLOTS; \
         design-symmetric-metadata §5.1.2)"
    )]
    RotorAtCap {
        appender: u32,
        held: u64,
        want: u64,
        cap: u64,
    },

    /// A grant of an UNLEASED slot whose records ring 0's window still
    /// holds after the bounded clearing cycles (design-symmetric-metadata
    /// §5.1.4's structural door — PR 4 review round 5 Issue 28, typed by
    /// round 6 Issue 30): the manager's structural records for the tree
    /// are not yet covered by the tail tree 0's next lessee is judged
    /// against. A SCHEDULE — a slow device, a long in-flight stage-B
    /// window — never a wedge: the tail MOVED (a tail that does not move
    /// at all is the stuck-reservation class, `Corrupt`), the volume is
    /// healthy, nothing was written; the requester retries (EAGAIN).
    /// Counted `slot_grant_deferrals`, never `manager_verb_refusals`.
    #[error(
        "grant of forest slot {slot} deferred: ring 0's window still holds the unleased tree's \
         records after {cycles} barriered checkpoint cycles (record frontier {frontier}, tail \
         {tail_start} → {tail}) — a schedule, not a wedge; retry (EAGAIN)"
    )]
    GrantDeferred {
        slot: u32,
        cycles: u32,
        frontier: u64,
        tail_start: u64,
        tail: u64,
    },

    /// A manager verb REJECTED at the service edge (design-symmetric-
    /// metadata §5.3.5; review round 1 Issue 2): a wire-carried integer
    /// names what the durable state cannot — a return run outside the
    /// volume, wider than the caller's record, an overflowing length.
    /// The buggy/hostile-peer class (`manager_verb_rejected`), kept apart
    /// from the witness refusals (`manager_verb_refusals`, must-stay-0).
    #[error("{0}")]
    Rejected(String),

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

    /// Symmetric PR 12b's joined door could not REACH the manager the
    /// heartbeat-fresh claim named (the dial failed, the connection reset)
    /// — the TRANSPORT class of the join, typed apart from [`Self::Busy`]
    /// (PR 13: a manager killed moments ago on this host is still exiting
    /// — its pid not yet provably dead, its listener resetting the dial —
    /// and the mount path re-reads the join target ONCE before it
    /// refuses; the D0 ladder's dead-pid proof then decides). Its errno is
    /// `EHOSTUNREACH` — the class the mount path keys on.
    #[error("{0}")]
    ManagerUnreachable(String),

    // PR 8 (design-symmetric-metadata §5.5.1 — the allocation lease's
    // ordering law): a successor asked for a DEAD holder's allocation
    // lease before the dead holder's home region was recovered (no
    // `recovered:` record yet). The durable state is healthy, nothing was
    // written, the requester RETRIES — the `GrantDeferred` class with the
    // lease's own words (wire: `STATUS_DEFERRED`).
    #[error("allocation lease deferred: {0}")]
    LeaseDeferred(String),

    // Symmetric PR 9 (design §5.1.4 — custody across a handover; review
    // round 2, Issues 5/9): a slot handover asked while a writer holds
    // custody of a file in the slot from this holder. The grants were
    // RECALLED through the S9 pull channel (the writer releases once the
    // file's pipeline is quiescent, within one renewal beat) and nothing
    // was written; the requester RETRIES — the `GrantDeferred` class with
    // the custody's own words (wire: `STATUS_DEFERRED`, `EAGAIN`).
    // Counted `slot_handover_custody_deferrals`.
    #[error("slot handover deferred for live custody: {0}")]
    HandoverDeferred(String),
}

/// Map KV-layer errors onto the crate error surface (mount / CLI / trait
/// callers): device I/O passes through untouched; the refusals that owe
/// userspace a specific errno carry it **structurally** (POSIX-6 — the
/// message is prose, never wire format); the remainder are
/// typed-refusals-turned-loud-messages, the `InvalidOperation` class the
/// mount gates use.
impl From<KvError> for crate::error::SqueezefsError {
    fn from(e: KvError) -> Self {
        use crate::error::SqueezefsError as E;
        match e {
            KvError::Io(inner) => inner,
            // §4.7 ENOSPC. The message reads "no space: …" (lower-case
            // n), which the retired substring rule (`contains("No
            // space")`) MISSED — a full metadata volume returned EINVAL
            // to `write(2)`. This is the POSIX-6 headline correction.
            e @ KvError::NoSpace { .. } => E::no_space(format!("kv metadata: {e}")),
            // `setxattr(2)`'s documented errno for an oversized value.
            // Only reachable below `XATTR_SIZE_MAX` (small-node volumes);
            // the kernel screens anything above it.
            e @ KvError::ValueTooLarge { .. } => E::too_large(format!("kv metadata: {e}")),
            // The D0 single-writer refusal names a holder — EBUSY.
            e @ KvError::Busy(_) => E::busy(format!("kv metadata: {e}")),
            // The join's transport class carries its OWN errno —
            // EHOSTUNREACH, the manager cannot be reached — so the mount
            // path classifies it structurally (`meta_backend::
            // join_dial_failed`), never by text.
            e @ KvError::ManagerUnreachable(_) => {
                E::refused(libc::EHOSTUNREACH, format!("kv metadata: {e}"))
            }
            // Out of grant with the manager unreachable: EAGAIN — the
            // caller retries inside the published stall bound.
            e @ KvError::GrantExhausted { .. } => {
                E::refused(libc::EAGAIN, format!("kv metadata: {e}"))
            }
            // A foreign slot lease at the commit door: EAGAIN — the ship
            // to the holder is PR 6/12's; the caller retries.
            // The classed retryable refusal (PR 13 review round 1, Issue 7):
            // the slot-moved decision is made on the CLASS, never the text.
            e @ KvError::SlotBusy { slot, holder, .. } => E::retryable(
                crate::error::RefusalClass::SlotMoved { slot, holder },
                format!("kv metadata: {e}"),
            ),
            // A grant deferred behind ring 0's window: the schedule's
            // EAGAIN — the requester's next ask finds the window clear.
            e @ KvError::GrantDeferred { .. } => {
                E::refused(libc::EAGAIN, format!("kv metadata: {e}"))
            }
            // PR 8: the allocation lease's re-grant waits on the home
            // recovery — the same EAGAIN class.
            e @ KvError::LeaseDeferred(_) => E::refused(libc::EAGAIN, format!("kv metadata: {e}")),
            // PR 9: a handover deferred for live custody — the same EAGAIN
            // class (the recalled writer releases within one renewal beat).
            e @ KvError::HandoverDeferred(_) => {
                E::refused(libc::EAGAIN, format!("kv metadata: {e}"))
            }
            // The rotor cap names a holder-side limit — EBUSY, like the
            // other manager refusals that name a holder.
            e @ KvError::RotorAtCap { .. } => E::busy(format!("kv metadata: {e}")),
            // A wire-invalid manager frame: the caller's argument is the
            // defect — EINVAL.
            e @ KvError::Rejected(_) => E::refused(libc::EINVAL, format!("kv metadata: {e}")),
            other => E::InvalidOperation(format!("kv metadata: {other}")),
        }
    }
}
