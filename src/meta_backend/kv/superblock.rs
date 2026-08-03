//! Superblock v3 and the sector-0 version gate (PR K6a; design §4.1
//! "Superblock v3", §5.1, §6.1, resolved OQ 1).
//!
//! Sector 0 keeps the historical v2 wire prefix — `magic: [u8; 8]` at
//! `[0..8)` and `version: u32` at `[8..12)` — so any binary (old or new)
//! can classify any SqueezeFS volume: pre-v3 binaries read a v3 volume's
//! version as 3 and refuse loud ("upgrade squeezefs"); this binary reads a
//! legacy v2 volume's version as ≤ 2 and refuses loud (v2 support removed
//! — reformat required, [`VolumeFormat::V2Legacy`]).
//!
//! ## Sector layout (little-endian, one 4 KiB sector)
//!
//! ```text
//! [0..8)     magic            "METALV01" (unchanged — version discriminates)
//! [8..12)    version: u32     3
//! [12..16)   node_size: u32   bytes; default 262144, format-time knob (§5.1)
//! [16..24)   features_incompat: u64   bit 0 = KV_V3; unknown ⇒ refuse mount
//! [24..32)   features_ro: u64         unknown ⇒ mount read-only (§4.11)
//! [32..48)   root_ledger  { start: u64, len: u64 }
//! [48..64)   journal      { start: u64, len: u64 }
//! [64..80)   alloc_bitmap { start: u64, len: u64 }
//! [80..96)   heap         { start: u64, len: u64 }
//! [96..112)  uuid: [u8; 16]
//! [112..120) hash_seed: u64   random at format; keys the §4.2 name hashes
//! [120..128) checksum: u64    xxh3_64 over the WHOLE sector, field zeroed
//! [128..4096) zero padding    covered by the checksum
//! ```
//!
//! The checksum covers the whole sector (not just the struct bytes, the
//! retired v2 convention) so a torn superblock write is detected no matter which bytes
//! the tear scrambled — the SB is a **durable-coverage unit** (§4.1): unlike
//! journal contents, a bad superblock fails the mount loud (§4.10 torn-SB
//! crash case).
//!
//! ## Tree-roots bootstrap
//!
//! The superblock deliberately carries **no tree roots** — those live in the
//! root ledger (§4.1) so checkpoints never rewrite sector 0. "Bootstrap" is
//! the `root_ledger` pointer: mount = SB → newest valid ledger record →
//! per-tree roots. The [`crate::meta_backend::kv::builder`] writes the
//! initial ledger record naming the fresh (empty or bulk-built) tree roots.

use super::alloc_ext::{bitmap_region_len, compaction_reserve_extents};
use super::checkpoint::ROOT_LEDGER_LEN;
use super::journal::{JOURNAL_PAGE_LEN, MAX_ENTRY_LEN};
use super::node::{record_value_cap, NodeLayout, NODE_PAGE};
use super::KvError;
use std::path::Path;

/// The metadata-volume magic at sector-0 offset 0 — shared with the
/// retired v2 format by design (the version field discriminates), so a
/// legacy volume still classifies as "SqueezeFS, unsupported version"
/// rather than "foreign".
pub const MAGIC_VALUE: &[u8; 8] = b"METALV01";

/// The 4 KiB metadata sector: the superblock's durable-coverage unit and
/// the journal ring's page size.
pub const SECTOR_SIZE: usize = 4096;

/// Byte offsets of the sector-0 wire layout (module docs). `magic` and
/// `version` sit at the v2 offsets by design — the downgrade gate.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 8;
const OFF_NODE_SIZE: usize = 12;
const OFF_FEAT_INCOMPAT: usize = 16;
const OFF_FEAT_RO: usize = 24;
const OFF_ROOT_LEDGER: usize = 32;
const OFF_JOURNAL: usize = 48;
const OFF_ALLOC_BITMAP: usize = 64;
const OFF_HEAP: usize = 80;
const OFF_UUID: usize = 96;
const OFF_HASH_SEED: usize = 112;
const OFF_CHECKSUM: usize = 120;
/// **DUR-5**: the superblock generation, carved out of the sector's zero
/// padding. It is covered by the existing whole-sector checksum and read
/// by nothing that predates DUR-5, so adding it is not a format change:
/// an older binary verifies and decodes the sector byte-identically and
/// simply ignores the field. Every writer bumps it; the reader resolves
/// primary vs [`backup_offset`] copy by newest-valid-wins.
const OFF_SB_GENERATION: usize = 128;

/// Format version this module writes and mounts.
pub const SUPERBLOCK_V3_VERSION: u32 = 3;

/// Whole-sector superblock image length.
pub const SUPERBLOCK_V3_LEN: usize = SECTOR_SIZE;

/// `features_incompat` bit 0: the KV v3 node layer (§4.1). Set on every
/// v3 volume.
pub const FEATURE_INCOMPAT_KV_V3: u64 = 1 << 0;

/// `features_incompat` bit 1: the root ledger carries the node-seq mint
/// watermark (Finding A, 2026-07-13 — mounts must reseed the mint
/// counter above every persisted frame stamp). Set on every volume
/// formatted since; pre-watermark v3 volumes refuse loud (their ledger
/// slots also no longer decode) — forward-only, reformat required.
/// `format --force` delivers that reformat: the format preflight
/// degrades every refused superblock class to its plain force gate
/// (`kv::builder::format_preflight` — refused classes cannot host live
/// current clients by construction), so this refusal never blocks the
/// remedy it demands.
pub const FEATURE_INCOMPAT_NODE_SEQ_WATERMARK: u64 = 1 << 1;

/// `features_incompat` bit 2: the volume participates in the **frozen
/// routing-width / slot-map machinery** (design-volume-lifecycle §5.5.1,
/// KD-7/KD-14, PR VL5a): its root-ledger records carry the §5.5.1a
/// membership stamp (and, from PR VL5b on, guest tree roots). Stamp-
/// extended ledger slots fail the pre-VL5a decoder's length-consistency
/// equation and **decode as absent** — an old binary would silently fall
/// back to an older slot (stale roots, stale `journal_tail_seq`) — so
/// the numbered §5.5.1a ordering invariant is normative: **(1)** this
/// bit is written and barriered durably **before (2)** the volume's
/// first stamp-extended ledger slot. A crash between (1) and (2) is
/// harmless: old binaries are already refused at this gate; this binary
/// proceeds (the stamp appears on the next stamped ledger write). Never
/// set on default formats — legacy sets stay bit-identical.
pub const FEATURE_INCOMPAT_KV_GUEST_SLOTS: u64 = 1 << 2;

/// `features_incompat` bit 3: the volume set has a **lifecycle history**
/// (design-volume-lifecycle KD-14, §7): a non-legacy volume record, a
/// non-identity slot map, or an active drain exists. Set durably at the
/// FIRST lifecycle commit — **before** the durable record it gates
/// (bit-before-durable-record ordering) — and never on untouched sets,
/// so legacy volumes stay bit-identical. Old binaries refuse loud via
/// the [`FEATURES_INCOMPAT_KNOWN`] gate.
pub const FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE: u64 = 1 << 3;

/// `features_incompat` bit 4: the volume carries **VL5b-extended
/// membership stamps** (explicit `native_slot` + per-slot ino cursors —
/// design-volume-lifecycle §5.5.2, PR VL5b) and possibly guest-keyspace
/// records. An extended stamp fails the VL5a decoder's
/// length-consistency equation and would **decode as absent** (silent
/// fallback to an older ledger slot — stale roots), so the §5.5.1a
/// ordering invariant recurs one generation later: **(1)** this bit is
/// written and barriered durably **before (2)** the volume's first
/// extended ledger slot (or first guest-keyspace record). VL5a-mask
/// binaries (bits 0..=3) refuse loud at this gate. Never set on
/// non-participating volumes — they stay byte-identical.
pub const FEATURE_INCOMPAT_KV_SLOT_MIGRATION: u64 = 1 << 4;

/// `features_incompat` bit 5: the volume may carry **layout delta
/// records** — `Delta`-kind records on `TREE_XATTRS` layout keys whose
/// payload is the write-commit-economy campaign's `LayoutDelta` wire
/// (`crate::layout_wire`, 2026-07-30). A pre-campaign binary's fold
/// would reject the payload as corruption at read/replay/compaction
/// time (its `InodeDelta::decode` refuses the magic), so the bit makes
/// the refusal a clean mount-time gate instead. Ordering invariant
/// (the KD-14 pattern): **(1)** this bit is written and barriered
/// durably **before (2)** the volume's first layout-delta journal
/// entry. A crash between (1) and (2) is harmless — old binaries are
/// refused with zero delta records present, this binary mounts
/// unchanged. Never set on volumes that never staged a delta — they
/// stay bit-identical.
pub const FEATURE_INCOMPAT_KV_LAYOUT_DELTAS: u64 = 1 << 5;

/// `features_incompat` bit 6: **dynamic meta routing**
/// (docs/design-dynamic-meta-routing.md; user ruling 2026-08-02): the
/// volume's membership stamps use the stride-run slot-set encoding over
/// the DERIVED routing width (`W = 2^16`, never a user knob), and
/// minting spreads across the per-volume mint set. Set at format on
/// EVERY volume (the `NODE_SEQ_WATERMARK` presence-required pattern):
/// a v3 volume WITHOUT this bit was formatted with the frozen
/// user-chosen routing width — its stamps carry the retired dense
/// slot-list wire this binary no longer decodes — and refuses loud,
/// reformat required (forward-only). One refusal covers both legacy
/// identity sets and `--meta-slots`-era stamped sets. Old binaries
/// refuse bit-6 volumes via their `FEATURES_INCOMPAT_KNOWN` gate (the
/// bit intersects no prior mask — pinned in
/// `tests/dynamic_meta_routing_tests.rs`).
pub const FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING: u64 = 1 << 6;

/// `features_incompat` bit 7: **durable writer term** (DLM stage S2 —
/// docs/pre-rc-engineering-spec.md §6.7 decision 4, §6.9, §6.11): the
/// volume's `writer_claim` carries a `term` field and a never-deleted
/// [`crate::meta_backend::kv::backend::WRITER_TERM_XATTR`] record holds
/// the durable era ladder, so the mount gate can bump the era before
/// arming and every fencing token this mount mints
/// (`(term << 40) | grant_seq`) dominates every token any predecessor
/// ever issued.
///
/// **Presence is OPTIONAL, deliberately** (unlike bit 6): a volume
/// WITHOUT this bit mounts exactly as it did pre-S2 — term 0, composed
/// token ≡ the bare grant sequence, claim bytes unchanged, no term
/// record written. The batched reformat window (execution plan Phase 8)
/// stamps existing volumes; **mount never stamps it** — fresh formats
/// carry it from [`SuperblockV3::plan`], and
/// [`set_durable_term_bit`] is the explicit upgrade path. Old binaries
/// refuse a bit-7 volume loud via [`FEATURES_INCOMPAT_KNOWN`] (the bit
/// intersects no prior mask), which is exactly right: they would mint
/// era-less tokens onto a volume whose records name eras.
pub const FEATURE_INCOMPAT_KV_DURABLE_TERM: u64 = 1 << 7;

/// `features_incompat` bit 8: **partitioned append** — the volume's
/// single-appender durable structures are expressed for N appenders
/// (docs/pre-rc-engineering-spec.md §6.2 items 2/3/4; execution-plan
/// rulings D8/D9):
///
/// * the journal ring is split into per-appender sub-rings whose page
///   headers carry an appender id, and replay merges the windows
///   ([`super::journal::replay_merge`]);
/// * the A/B extent bitmap is partitioned by page, with per-appender free
///   budgets, compaction reserves, pending-free FIFOs, and durable-coverage
///   clocks;
/// * the 32-slot root ledger is split into per-appender slot ranges, and
///   its records carry an append-partition suffix.
///
/// **Presence is OPTIONAL and this binary NEVER STAMPS IT** (ruling D9,
/// the bit-7 posture taken one step further): [`SuperblockV3::plan`] does
/// not set it, mount does not set it, and no runtime path sets it — it is
/// stamped by the Phase-8 batched reformat window alongside the other §6.2
/// format changes. A volume without the bit behaves EXACTLY as today: solo
/// rings, one bitmap, `slot = seq % 32`, suffix-less ledger records — every
/// structure byte-identical (pinned by
/// `tests/kv_partitioned_append_tests.rs`).
///
/// Old binaries refuse a bit-8 volume loud via their own
/// [`FEATURES_INCOMPAT_KNOWN`] gate (the bit intersects no prior mask),
/// which is exactly right: they would append into writer 0's sub-ring
/// while believing it is the whole ring, and their `slot = seq % 32`
/// checkpoints would overwrite every peer's ledger range.
pub const FEATURE_INCOMPAT_KV_PARTITIONED_APPEND: u64 = 1 << 8;
/// `features_incompat` bit 8: **durable data-block reference accounting**
/// (pre-RC engineering spec §6.2 **item 1**, "the largest item"; ruling
/// **D9**): the volume carries the
/// [`crate::meta_backend::kv::record::TREE_BLOCK_REFS`] tree — one
/// checksummed, CoW, journaled record per `(data volume, block, owner
/// ino, map index)` reference — so block refcounts and the free list are
/// **durable shared ownership accounting** instead of state re-derived by
/// a full inode-tree walk at every mount.
///
/// **Presence is OPTIONAL, deliberately** (the bit-7 pattern, not bit
/// 6's presence-required one): a volume WITHOUT this bit mounts exactly
/// as it did before the bit existed — the mount-time layout walk
/// (`recover_active_blocks_v3`) rebuilds the RAM refcount map and free
/// list, no fourth tree root is minted, no accounting record is ever
/// staged, and the superblock is not rewritten.
///
/// **Nothing stamps it today** — not mount, and not
/// [`SuperblockV3::plan`] either (ruling D9: build the bit, do not stamp
/// it). [`set_block_refcounts_bit`] is the sole stamping path, for the
/// batched Phase-8 reformat window. A fresh format therefore mounts
/// DERIVED, which is the safe default while the write-path wiring is
/// incomplete: a partially-populated ledger is NON-empty, so the
/// "an empty population is never authoritative" rule does not fire, and
/// every reference an unwired site failed to stage would read back as a
/// free block. Derived accounting cannot fail that way — it re-reads the
/// layouts, which are always complete.
///
/// Old binaries refuse a bit-9 volume loud via
/// [`FEATURES_INCOMPAT_KNOWN`] — exactly right: they would free blocks
/// and run W1 sole-owner patches against accounting they never maintain,
/// silently diverging the durable ledger from the layouts.
/// **Bit 9, not 8** — bit 8 is [`FEATURE_INCOMPAT_KV_PARTITIONED_APPEND`].
/// Both were authored in parallel against the same free bit; partitioned
/// append landed first, so durable block references took the next one.
pub const FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS: u64 = 1 << 9;

/// Bit index of [`FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING`] — named so
/// refusal messages can print it without re-deriving a shift.
pub const WRITER_SCOPED_STAGING_BIT: u32 = 10;

/// `features_incompat` bit 10: **writer-scoped staging** (pre-RC
/// engineering spec §6.2 **items 8 and 10**; ruling **D9**). The set's
/// node-private staged payloads are LABELLED with the writing node, in
/// both places where the label is needed:
///
/// * **item 8, the record level** — `active_block:`,
///   `active_block_ext:` and `mapping:` keys carry a trailing
///   `:w_{16 hex}` writer-scope component
///   ([`crate::writer_scope::scoped_key_suffix`]), appended AFTER the
///   existing identity components so every historical scan prefix
///   (`active_block:inode_{ino}:`) keeps its meaning — the same
///   reservation `TREE_BLOCK_REFS` made for a writer id after
///   `block_index` (docs/design-durable-block-refcounts.md §3.2);
/// * **item 10, the root level** — the staging generation marker binds
///   `{volume-set generation}@node:{16 hex}` instead of the set
///   generation alone, so a peer's staging root can no longer pass this
///   node's generation gate.
///
/// The two engage from ONE bit because a half-engaged state is unsound in
/// both directions: labelled records under a node-blind root gate still
/// let a peer's whole root be adopted, and a node-scoped root whose
/// records are unlabelled cannot classify the records inside it.
///
/// **Presence is OPTIONAL and this binary NEVER STAMPS IT** (ruling D9,
/// the bit-7/8/9 posture): [`SuperblockV3::plan`] does not set it, mount
/// does not set it, and no runtime path sets it —
/// [`set_writer_scoped_staging_bit`] is the sole stamping path, for the
/// Phase-8 batched reformat window. A volume without the bit behaves
/// EXACTLY as today: byte-identical keys, byte-identical marker bytes,
/// and pre-change staged work recovers unchanged (pinned by
/// `tests/writer_scoped_staging_tests.rs`).
///
/// Old binaries refuse a bit-10 volume loud via their own
/// [`FEATURES_INCOMPAT_KNOWN`] gate (the bit intersects no prior mask),
/// which is exactly right: they would mint UNSCOPED keys onto a set whose
/// peers' records are scoped, and their generation gate would adopt or
/// wipe any staging root of the set regardless of which node populated
/// it.
///
/// **Bit 10, not 8 or 9** — bit 8 is
/// [`FEATURE_INCOMPAT_KV_PARTITIONED_APPEND`] and bit 9 is
/// [`FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS`]; both were once claimed in
/// parallel against the same free bit, which is silent on-disk aliasing.
/// Disjointness of the whole set is pinned as a test
/// (`incompat_bits_are_single_bit_and_pairwise_disjoint`), so a repeat is
/// a red gate rather than a field mystery.
pub const FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING: u64 = 1 << WRITER_SCOPED_STAGING_BIT;

/// `features_incompat` bit 11: **multi-writer data plane** (DLM stage
/// **S7** — pre-RC engineering spec §6.9 S7/S9 rows, §6.7 "On external
/// consensus"): the volume set's format is expressed for more than one
/// concurrent data-plane writer — per-writer `active_block:` key scoping,
/// durable shared block accounting, and the custody records S8/S9 add.
///
/// The bit is a **capability gate, not a structure**: `data_custody`'s
/// multi-writer arming refuses a set that does not carry it, so a mount
/// can never arm a device-enforced multi-writer data plane over a format
/// whose recovery paths assume one writer.
///
/// **Presence is OPTIONAL and nothing stamps it** (ruling **D9**, the
/// bit-7/8/9 posture): [`SuperblockV3::plan`] does not set it, mount does
/// not set it, and no runtime path sets it — [`set_multi_writer_data_bit`]
/// is the sole stamping path, for the Phase-8 window that lands the §6.2
/// format changes together. A volume without it behaves exactly as today
/// and `SQUEEZEFS_MULTI_WRITER=1` refuses the mount loud, naming the bit.
///
/// Old binaries refuse a bit-11 volume loud via their own
/// [`FEATURES_INCOMPAT_KNOWN`] gate — exactly right: they would recover a
/// multi-writer set's node-private staging and block accounting as if
/// every record were their own.
/// **Bit 11, not 10** — S7 was built in parallel with §6.2 items 8/10 and
/// both branches independently claimed bit 10, the second such collision
/// in this program (bits 8/9 were the first). Bit 10 belongs to
/// [`FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING`], which merged first;
/// this bit renumbered at integration. The disjointness pin
/// (`incompat_bits_are_single_bit_and_pairwise_disjoint`) is what makes
/// the next one a red gate instead of silent on-disk aliasing.
pub const FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA: u64 = 1 << 11;

/// `features_incompat` bit 12: **per-writer ino lanes** — the volume's ino
/// namespace is partitioned into one monotone LANE per appender, so two
/// writers can mint concurrently without ever producing the same ino
/// (pre-RC engineering spec §6.2 **item 5**, "cheapest of the five";
/// rulings **D8**/**D9**; design `docs/design-mw-cursors-and-incarnation.md`).
///
/// The law: with `writers = W`, a local ino `i` belongs to lane
/// `(i − 2) % W`, and appender `w` mints only lane-`w` locals (stride `W`
/// from a lane-aligned floor). Solo (`W = 1`) is lane 0 = every ino, i.e.
/// today's dense `fetch_add(1)` watermark **arithmetically unchanged**.
/// Nothing else moves: global inos still route through the frozen
/// `routing_width` (`route_ino_width`), so st_ino stability is untouched,
/// and the durable per-writer watermark is each appender's OWN root-ledger
/// record (`next_ino` in its own slot range, which incompat bit 8's
/// partitioned ledger already provides) — this bit adds **no new durable
/// field**.
///
/// **Presence is OPTIONAL and this binary NEVER STAMPS IT** (ruling D9,
/// the bit-8/9 posture): [`SuperblockV3::plan`] does not set it, mount does
/// not set it, and no runtime path sets it — the Phase-8 batched reformat
/// window owns that act ([`set_ino_lanes_bit`] is the sole stamping path).
/// A volume without the bit mints dense inos exactly as today, and a
/// non-solo lane is REFUSED loud on it (pinned by
/// `tests/mw_ino_lane_tests.rs`).
///
/// Old binaries refuse a bit-12 volume loud via their own
/// [`FEATURES_INCOMPAT_KNOWN`] gate — exactly right: a lane-unaware writer
/// mints DENSE inos over every peer's lane, and a peer resuming from its
/// own durable watermark would then re-mint an ino that writer already
/// used. Duplicate inos alias files immediately, and the daemon's IPC
/// binding table rests on the monotonic never-reused ino law, so the alias
/// reaches fd bindings too.
///
/// **Bit 12, not 10 — and 10/11 are RESERVED, not free.** Bit 10 is
/// writer-scoped staging keys (§6.2 item 8) and bit 11 the S7 data-plane
/// fence; both were authored on branches parallel to this one, so items 5
/// and 6 moved up to 12/13 rather than alias them. The mask gap below 12 is
/// therefore deliberate: never fill 10 or 11 from here. (Third occurrence
/// of the bit-8 collision class — two definitions of one bit is silent
/// on-disk aliasing, pinned by
/// `tests/mw_ino_lane_tests.rs::the_multi_writer_format_bits_are_disjoint_and_unstamped`.)
pub const FEATURE_INCOMPAT_KV_INO_LANES: u64 = 1 << 12;

/// `features_incompat` bit 13: **`offset ‖ incarnation` block keys** — a
/// persisted block key names not just WHERE a block lives but WHICH
/// lifetime of that device offset it is (pre-RC engineering spec §6.2
/// **item 6**, rationale §6.3 "block-key binding"; design
/// `docs/design-mw-cursors-and-incarnation.md`).
///
/// Today a block key is a bare, reusable device offset, and the read
/// path's serve proof — *bytes for key K serve for block b iff the fetch
/// was incarnation-valid AND the current map still binds b → K* — rests on
/// two PROCESS-LOCAL premises. So when node A overwrites a block, frees
/// the offset, and the allocator reissues it to another file, node B —
/// whose cached map still binds b → K and whose incarnation word is
/// untouched — serves the other file's bytes with **no error and no
/// counter** (silent on a passthrough volume; on a transformed volume the
/// AEAD tag fails, the one honest degradation). Carrying the incarnation
/// IN the key makes that stale binding structurally detectable: the key
/// itself disagrees with the offset's live lifetime.
///
/// Wire form: `[be://]offset@<base36 incarnation>` — the incarnation is a
/// suffix on the OFFSET component, so it survives
/// [`crate::routing::clean_block_key`] unchanged, the `:rel:len`
/// decoration still parses after it, and the W1 whole-block predicate is
/// unaffected. `incarnation == 0` is the absent/legacy form, which is
/// exactly today's bare key — so an un-stamped volume's keys are
/// **byte-identical**.
///
/// The lifetime stamp is `(writer_term << 40) | lane_seq`: the SAME
/// composition the DLM's S2 fencing token uses (spec §6.7 decision 4), so
/// it needs no new durable record — the durable, barriered-before-arm
/// [`super::backend::WRITER_TERM_XATTR`] era is what makes an incarnation
/// unrepeatable across a remount, and the lane component is what makes it
/// unrepeatable across appenders. This bit therefore REQUIRES
/// [`FEATURE_INCOMPAT_KV_DURABLE_TERM`] (bit 7): without the durable era
/// the stamp would restart at every mount and the detection would be a
/// lie. Engaging it on a term-less volume is refused loud.
///
/// **Presence is OPTIONAL and this binary NEVER STAMPS IT** (ruling D9):
/// [`SuperblockV3::plan`] does not set it, mount does not set it,
/// [`set_block_key_incarnation_bit`] is the Phase-8 path. Old binaries
/// refuse a bit-13 volume loud — exactly right: they would mint bare keys
/// onto a volume whose keys name lifetimes, and free/patch offsets whose
/// stale-binding refusals they cannot perform.
///
/// **Bit 13, not 11** — see the reservation note on
/// [`FEATURE_INCOMPAT_KV_INO_LANES`]: bits 10 and 11 belong to parallel
/// branches (writer-scoped staging, the S7 data-plane fence).
pub const FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION: u64 = 1 << 13;

/// Incompat feature bits this binary understands. Any other set bit
/// refuses the mount naming the bit (§6.1).
pub const FEATURES_INCOMPAT_KNOWN: u64 = FEATURE_INCOMPAT_KV_V3
    | FEATURE_INCOMPAT_NODE_SEQ_WATERMARK
    | FEATURE_INCOMPAT_KV_GUEST_SLOTS
    | FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE
    | FEATURE_INCOMPAT_KV_SLOT_MIGRATION
    | FEATURE_INCOMPAT_KV_LAYOUT_DELTAS
    | FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING
    | FEATURE_INCOMPAT_KV_DURABLE_TERM
    | FEATURE_INCOMPAT_KV_PARTITIONED_APPEND
    | FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS
    | FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING
    | FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA
    | FEATURE_INCOMPAT_KV_INO_LANES
    | FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION;

/// Read-only feature bits this binary understands (none yet — §4.11
/// reserves the mechanism for snapshots). Unknown bits mount read-only.
pub const FEATURES_RO_KNOWN: u64 = 0;

/// Resolved OQ 1 journal-ring clamp floor: 8 MiB.
pub const JOURNAL_RING_MIN: u64 = 8 * 1024 * 1024;

/// Resolved OQ 1 journal-ring clamp ceiling: 32 MiB.
pub const JOURNAL_RING_MAX: u64 = 32 * 1024 * 1024;

/// One on-disk extent `[start, start + len)` named by the superblock
/// (§4.1: "no hardcoded offsets except sector 0").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentRef {
    pub start: u64,
    pub len: u64,
}

impl ExtentRef {
    /// Exclusive end offset.
    pub fn end(&self) -> u64 {
        self.start + self.len
    }
}

/// The v3 superblock (§4.1). `magic`, `version`, and `checksum` are wire
/// artifacts owned by [`SuperblockV3::encode_sector`] /
/// [`SuperblockV3::decode_sector`]; the struct carries the format-time
/// decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperblockV3 {
    /// Node/extent size in bytes (validated: 4 KiB multiple in
    /// [64 KiB, 1 MiB] — [`super::node::NodeLayout`]).
    pub node_size: u32,
    /// §4.1 feature bits; bit 0 ([`FEATURE_INCOMPAT_KV_V3`]) always set.
    pub features_incompat: u64,
    /// §4.11 read-only feature bits.
    pub features_ro: u64,
    /// The 32 × 4 KiB root-ledger slot array (§4.1).
    pub root_ledger: ExtentRef,
    /// The journal ring (sized by the resolved OQ 1 clamp at format).
    pub journal: ExtentRef,
    /// A/B allocator bitmap page pairs (§4.7).
    pub alloc_bitmap: ExtentRef,
    /// The node heap; `heap.len / node_size` extents.
    pub heap: ExtentRef,
    pub uuid: [u8; 16],
    /// Per-volume secret seed keying the §4.2 dentry/xattr name hashes.
    pub hash_seed: u64,
}

impl SuperblockV3 {
    /// Plan a fresh volume's geometry (format time): ledger at 4096,
    /// journal ring per the resolved OQ 1 clamp (`journal_len_override`
    /// replaces the clamp — the `--meta-journal-mb` CLI contract, §5.1),
    /// bitmap sized for the heap, heap aligned to `node_size`. Errors
    /// typed and loud when the volume cannot hold the fixed structures
    /// plus at least one usable extent beyond the §4.7 compaction reserve.
    pub fn plan(
        volume_len: u64,
        node_size: usize,
        journal_len_override: Option<u64>,
        uuid: [u8; 16],
        hash_seed: u64,
    ) -> Result<Self, KvError> {
        let layout = NodeLayout::new(node_size)?;
        let node_size = layout.node_size() as u64;

        let root_ledger = ExtentRef {
            start: SECTOR_SIZE as u64,
            len: ROOT_LEDGER_LEN,
        };

        let journal_len = match journal_len_override {
            Some(len) => {
                // Ring floor: the reserve carve-out (max(256 KiB, ring/64))
                // plus one max-size entry must always be admissible, or a
                // legal transaction could never commit (§4.4 pt 5).
                let floor = 256 * 1024 + MAX_ENTRY_LEN;
                if len % JOURNAL_PAGE_LEN != 0 || len < floor {
                    return Err(KvError::Corrupt(format!(
                        "journal ring override {len} bytes is invalid: must be a 4 KiB \
                         multiple of at least {floor} bytes (reserve + one max entry)"
                    )));
                }
                len
            }
            None => journal_ring_len(volume_len),
        };
        let journal = ExtentRef {
            start: root_ledger.end(),
            len: journal_len,
        };

        // Bitmap region sized for the extent-count upper bound (the heap
        // cannot exceed volume/node_size extents); the actual count is
        // recomputed below once the heap start is fixed. Over-reserving a
        // page pair or two is deliberate — it breaks the mutual
        // dependency between bitmap size and heap size.
        let upper_extents = volume_len / node_size;
        let alloc_bitmap = ExtentRef {
            start: journal.end(),
            len: bitmap_region_len(upper_extents),
        };

        // DUR-5: hold back the LAST aligned sector for the redundant
        // superblock copy. Costs at most one heap extent and is what
        // makes the copy guaranteed-present on every fresh format (the
        // geometry is self-describing, so an older binary mounts such a
        // volume unchanged — it just sees a marginally smaller heap).
        let heap_start = alloc_bitmap.end().div_ceil(node_size) * node_size;
        let usable = volume_len.saturating_sub(SUPERBLOCK_V3_LEN as u64);
        let total_extents = usable.saturating_sub(heap_start) / node_size;
        // A usable volume must hold the §4.7 compaction reserve plus room
        // for the three tree roots and growth.
        let min_extents = compaction_reserve_extents(total_extents) + 4;
        if total_extents < min_extents {
            return Err(KvError::Corrupt(format!(
                "metadata volume too small for format v3: {volume_len} bytes leaves \
                 {total_extents} heap extents of {node_size} bytes after the fixed structures \
                 (superblock + ledger + {journal_len}-byte journal + bitmap end at \
                 {heap_start}); at least {min_extents} extents are required — grow the volume \
                 or pass a smaller --meta-journal-mb / --meta-node-kib"
            )));
        }
        let heap = ExtentRef {
            start: heap_start,
            len: total_extents * node_size,
        };

        Ok(Self {
            node_size: node_size as u32,
            // Every fresh format is dynamic-routing (bit 6 — presence
            // REQUIRED at decode, the NODE_SEQ_WATERMARK pattern) and
            // durable-term (bit 7 — presence OPTIONAL: pre-S2 volumes keep
            // mounting era-less until the batched reformat window stamps
            // them); the stamp bits (2/4) ride the builder's stamped image
            // path.
            //
            // **Bit 8 (durable block refcounts) is deliberately NOT here**
            // — ruling D9: build the bit, do not stamp it. A fresh format
            // mounts with DERIVED block accounting, exactly like a bit-2/4
            // volume, and [`set_block_refcounts_bit`] is the Phase-8
            // upgrade path.
            //
            // This is a SAFETY property, not just discipline. The durable
            // ledger is only as complete as the set of write-path sites
            // that stage into it, and while that wiring is unfinished a
            // partially-populated ledger is the dangerous state: it is
            // NON-empty, so the "an empty population is never
            // authoritative" rule does not fire, and every reference an
            // unwired site failed to stage reads back as a FREE block —
            // `recover_block` then hands live data to the next writer.
            // Derived accounting has no such failure mode: it re-reads the
            // layouts, which are always complete. Re-adding the bit here is
            // gated on the oracle running clean across the write-path
            // suites (docs/design-durable-block-refcounts.md §11).
            features_incompat: FEATURE_INCOMPAT_KV_V3
                | FEATURE_INCOMPAT_NODE_SEQ_WATERMARK
                | FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING
                | FEATURE_INCOMPAT_KV_DURABLE_TERM,
            features_ro: 0,
            root_ledger,
            journal,
            alloc_bitmap,
            heap,
            uuid,
            hash_seed,
        })
    }

    /// Heap extent count (`heap.len / node_size`).
    pub fn total_extents(&self) -> u64 {
        self.heap.len / u64::from(self.node_size)
    }

    /// Journal ring page count (`journal.len / 4096`).
    pub fn journal_pages(&self) -> u64 {
        self.journal.len / JOURNAL_PAGE_LEN
    }

    /// Incompat feature bits set on disk that this binary does not
    /// understand (nonzero ⇒ the mount was refused by
    /// [`classify_sector0`]; kept for error surfaces and tests).
    pub fn unknown_incompat(&self) -> u64 {
        self.features_incompat & !FEATURES_INCOMPAT_KNOWN
    }

    /// Read-only feature bits set on disk that this binary does not
    /// understand (nonzero ⇒ mount read-only once K6b has a write path
    /// to withhold; K6a's read side logs it).
    pub fn unknown_ro(&self) -> u64 {
        self.features_ro & !FEATURES_RO_KNOWN
    }

    /// Encode into a checksummed whole-sector image (generation 0 — the
    /// format-time image; runtime writers use
    /// [`Self::encode_sector_at_generation`]).
    pub fn encode_sector(&self) -> Result<Vec<u8>, KvError> {
        self.encode_sector_at_generation(0)
    }

    /// [`Self::encode_sector`] stamped with a DUR-5 superblock
    /// generation (see [`OFF_SB_GENERATION`]).
    pub fn encode_sector_at_generation(&self, generation: u64) -> Result<Vec<u8>, KvError> {
        let mut img = self.encode_sector_body()?;
        img[OFF_SB_GENERATION..OFF_SB_GENERATION + 8].copy_from_slice(&generation.to_le_bytes());
        let sum = sector_checksum(&img);
        img[OFF_CHECKSUM..OFF_CHECKSUM + 8].copy_from_slice(&sum.to_le_bytes());
        Ok(img)
    }

    /// The sector image with every field but the checksum populated.
    fn encode_sector_body(&self) -> Result<Vec<u8>, KvError> {
        self.validate_geometry()?;
        let mut img = vec![0u8; SUPERBLOCK_V3_LEN];
        img[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(MAGIC_VALUE);
        img[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&SUPERBLOCK_V3_VERSION.to_le_bytes());
        img[OFF_NODE_SIZE..OFF_NODE_SIZE + 4].copy_from_slice(&self.node_size.to_le_bytes());
        img[OFF_FEAT_INCOMPAT..OFF_FEAT_INCOMPAT + 8]
            .copy_from_slice(&self.features_incompat.to_le_bytes());
        img[OFF_FEAT_RO..OFF_FEAT_RO + 8].copy_from_slice(&self.features_ro.to_le_bytes());
        for (off, ext) in [
            (OFF_ROOT_LEDGER, &self.root_ledger),
            (OFF_JOURNAL, &self.journal),
            (OFF_ALLOC_BITMAP, &self.alloc_bitmap),
            (OFF_HEAP, &self.heap),
        ] {
            img[off..off + 8].copy_from_slice(&ext.start.to_le_bytes());
            img[off + 8..off + 16].copy_from_slice(&ext.len.to_le_bytes());
        }
        img[OFF_UUID..OFF_UUID + 16].copy_from_slice(&self.uuid);
        img[OFF_HASH_SEED..OFF_HASH_SEED + 8].copy_from_slice(&self.hash_seed.to_le_bytes());
        Ok(img)
    }

    /// Decode + verify a sector-0 image already known to carry version 3:
    /// magic, checksum over the whole sector, bounds-checked geometry
    /// (§9: every length validated before use), feature gate
    /// (unknown incompat bits refuse loud, naming the bits). Torn or
    /// tampered superblocks fail loud — the §4.10 torn-SB crash case.
    pub fn decode_sector(buf: &[u8]) -> Result<Self, KvError> {
        Self::decode_sector_with_known(buf, FEATURES_INCOMPAT_KNOWN)
    }

    /// [`Self::decode_sector`] against an explicit known-incompat mask.
    /// The production gate passes [`FEATURES_INCOMPAT_KNOWN`]; tests pass
    /// historical masks to prove the forward-only refusal an OLD binary
    /// would issue for newer bits (KD-14's pinned old-mask check — the
    /// real gate code path, not a synthetic "some bit set" assertion).
    pub fn decode_sector_with_known(buf: &[u8], known_incompat: u64) -> Result<Self, KvError> {
        if buf.len() != SUPERBLOCK_V3_LEN {
            return Err(KvError::Corrupt(format!(
                "v3 superblock sector must be {SUPERBLOCK_V3_LEN} bytes, got {}",
                buf.len()
            )));
        }
        if &buf[OFF_MAGIC..OFF_MAGIC + 8] != MAGIC_VALUE {
            return Err(KvError::Corrupt(
                "bad v3 superblock magic (corrupted or foreign volume)".to_string(),
            ));
        }
        let version = u32::from_le_bytes(buf[OFF_VERSION..OFF_VERSION + 4].try_into().unwrap());
        if version != SUPERBLOCK_V3_VERSION {
            return Err(KvError::Corrupt(format!(
                "v3 decoder handed superblock version {version} (classification bug)"
            )));
        }
        let stored = u64::from_le_bytes(buf[OFF_CHECKSUM..OFF_CHECKSUM + 8].try_into().unwrap());
        let computed = sector_checksum(buf);
        if stored != computed {
            // Not the bare ChecksumMismatch variant: its display names
            // bsets, and a torn superblock deserves its own words.
            return Err(KvError::Corrupt(format!(
                "superblock checksum mismatch: stored {stored:#018x}, computed {computed:#018x} \
                 — corrupted superblock"
            )));
        }
        let ext_at = |off: usize| ExtentRef {
            start: u64::from_le_bytes(buf[off..off + 8].try_into().unwrap()),
            len: u64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap()),
        };
        let sb = Self {
            node_size: u32::from_le_bytes(
                buf[OFF_NODE_SIZE..OFF_NODE_SIZE + 4].try_into().unwrap(),
            ),
            features_incompat: u64::from_le_bytes(
                buf[OFF_FEAT_INCOMPAT..OFF_FEAT_INCOMPAT + 8]
                    .try_into()
                    .unwrap(),
            ),
            features_ro: u64::from_le_bytes(buf[OFF_FEAT_RO..OFF_FEAT_RO + 8].try_into().unwrap()),
            root_ledger: ext_at(OFF_ROOT_LEDGER),
            journal: ext_at(OFF_JOURNAL),
            alloc_bitmap: ext_at(OFF_ALLOC_BITMAP),
            heap: ext_at(OFF_HEAP),
            uuid: buf[OFF_UUID..OFF_UUID + 16].try_into().unwrap(),
            hash_seed: u64::from_le_bytes(
                buf[OFF_HASH_SEED..OFF_HASH_SEED + 8].try_into().unwrap(),
            ),
        };
        // Feature gate (§6.1): the KV_V3 bit must be present; unknown
        // incompat bits refuse the mount naming the bits. Unknown ro bits
        // pass — their read-only semantics belong to callers with a write
        // path (§4.11).
        if sb.features_incompat & FEATURE_INCOMPAT_KV_V3 == 0 {
            return Err(KvError::Corrupt(
                "v3 superblock without the KV_V3 incompat bit (corrupt feature field)".to_string(),
            ));
        }
        if sb.features_incompat & FEATURE_INCOMPAT_NODE_SEQ_WATERMARK == 0 {
            return Err(KvError::Corrupt(
                "pre-watermark v3 volume: formatted before the node-seq mint watermark \
                 (Finding A) and no longer supported; reformat required"
                    .to_string(),
            ));
        }
        // Dynamic-meta-routing gate (design-dynamic-meta-routing §6,
        // forward-only): checked only when THIS binary's mask knows bit 6
        // — the old-mask refusal tests exercise pre-campaign gates whose
        // binaries had no such requirement, and `known_incompat` is
        // exactly the mask that models the deciding binary.
        if known_incompat & FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING != 0
            && sb.features_incompat & FEATURE_INCOMPAT_KV_DYNAMIC_ROUTING == 0
        {
            return Err(KvError::Corrupt(
                "v3 volume formatted with a frozen routing width (pre-dynamic-meta-routing) \
                 — no longer supported: routing widths are derived now, never chosen; \
                 reformat required (`squeezefs format --force`, destroys the old contents)"
                    .to_string(),
            ));
        }
        let unknown = sb.features_incompat & !known_incompat;
        if unknown != 0 {
            let bits: Vec<String> = (0..64)
                .filter(|b| unknown & (1u64 << b) != 0)
                .map(|b| format!("bit {b}"))
                .collect();
            return Err(KvError::Corrupt(format!(
                "unknown incompatible feature bits on v3 superblock: {} \
                 ({unknown:#x}) — upgrade squeezefs to mount this volume",
                bits.join(", ")
            )));
        }
        sb.validate_geometry()?;
        Ok(sb)
    }

    /// §9 bounds discipline: every geometry field validated before any
    /// caller dereferences it. The ledger | journal | bitmap triple must
    /// ascend without overlap and above sector 0; lengths must match their
    /// consumers' fixed shapes; and the heap must be whole aligned extents
    /// covering the bitmap's bit range.
    ///
    /// Two legal placements of that fixed triple relative to the heap
    /// (design §4.1 "no hardcoded offsets except sector 0 — placed wherever
    /// free space exists"):
    ///
    /// * **fresh format** ([`SuperblockV3::plan`]) — `SB | ledger | journal
    ///   | bitmap | heap`, the triple entirely *below* `heap.start`;
    /// * **in-heap triple** (historically produced by the retired v2→v3
    ///   in-place migrator; still a legal v3 geometry — the on-disk format
    ///   is unchanged by the migrator's removal) — the heap spans the whole
    ///   device and the fixed triple lives in *reserved extents inside it*
    ///   (the bitmap marks those extents allocated so the allocator never
    ///   hands them out; that is the image producer's contract, not
    ///   something this structural check can see). Then the triple is
    ///   entirely within `[heap.start, heap.end)` and
    ///   `heap.start ≤ ledger.start`.
    ///
    /// Both are accepted; anything else is corruption.
    fn validate_geometry(&self) -> Result<(), KvError> {
        let layout = NodeLayout::new(self.node_size as usize)?;
        let node_size = layout.node_size() as u64;
        let corrupt = |msg: String| Err(KvError::Corrupt(format!("v3 superblock geometry: {msg}")));

        if self.root_ledger.start < SECTOR_SIZE as u64 {
            return corrupt(format!(
                "root ledger at {} overlaps sector 0",
                self.root_ledger.start
            ));
        }
        if self.root_ledger.len != ROOT_LEDGER_LEN {
            return corrupt(format!(
                "root ledger length {} != the fixed {ROOT_LEDGER_LEN}",
                self.root_ledger.len
            ));
        }
        if self.journal.start < self.root_ledger.end()
            || self.journal.len == 0
            || self.journal.len % JOURNAL_PAGE_LEN != 0
        {
            return corrupt(format!(
                "journal [{}, +{}) must follow the ledger in whole 4 KiB pages",
                self.journal.start, self.journal.len
            ));
        }
        if self.alloc_bitmap.start < self.journal.end() {
            return corrupt(format!(
                "alloc bitmap at {} overlaps the journal",
                self.alloc_bitmap.start
            ));
        }
        if self.heap.start % NODE_PAGE as u64 != 0
            || self.heap.len < node_size
            || self.heap.len % node_size != 0
        {
            return corrupt(format!(
                "heap [{}, +{}) must be whole {node_size}-byte extents on a {NODE_PAGE}-byte \
                 boundary",
                self.heap.start, self.heap.len
            ));
        }
        // The fixed ledger/journal/bitmap triple (already validated ascending
        // & non-overlapping above) is placed against the heap in one of the
        // two legal ways documented on this fn: entirely below it (fresh
        // format) or entirely within it on reserved extents (migrated volume,
        // §6.2). Anything else is corruption.
        let triple_below_heap = self.alloc_bitmap.end() <= self.heap.start;
        let triple_within_heap =
            self.heap.start <= self.root_ledger.start && self.alloc_bitmap.end() <= self.heap.end();
        if !(triple_below_heap || triple_within_heap) {
            return corrupt(format!(
                "fixed structures (ledger/journal/bitmap ending at {}) must sit either entirely \
                 below heap.start {} (fresh format) or entirely within the heap [{}, {}) on \
                 reserved extents (migrated volume) — got neither",
                self.alloc_bitmap.end(),
                self.heap.start,
                self.heap.start,
                self.heap.end()
            ));
        }
        if self.alloc_bitmap.len < bitmap_region_len(self.total_extents()) {
            return corrupt(format!(
                "alloc bitmap length {} cannot cover {} extents",
                self.alloc_bitmap.len,
                self.total_extents()
            ));
        }
        // The heap must clear the §4.7 compaction reserve with headroom —
        // the same floor `plan` enforces. Without this, a crafted (still
        // checksummed) superblock could hand the allocator a reserve ≥
        // total and turn a mount into a construction assert.
        let total = self.total_extents();
        let min_extents = compaction_reserve_extents(total) + 4;
        if total < min_extents {
            return corrupt(format!(
                "heap of {total} extents cannot hold the compaction reserve \
                 (≥ {min_extents} required)"
            ));
        }
        Ok(())
    }
}

/// xxh3 over the whole sector image with the checksum field zeroed.
fn sector_checksum(img: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&img[..OFF_CHECKSUM]);
    h.update(&[0u8; 8]);
    h.update(&img[OFF_CHECKSUM + 8..]);
    h.digest()
}

/// The resolved OQ 1 ring default: `clamp(volume_len / 64, 8 MiB, 32 MiB)`,
/// rounded down to whole 4 KiB pages (the clamp bounds already are, so the
/// alignment can never dip below the floor).
pub fn journal_ring_len(volume_len: u64) -> u64 {
    let clamped = (volume_len / 64).clamp(JOURNAL_RING_MIN, JOURNAL_RING_MAX);
    clamped / JOURNAL_PAGE_LEN * JOURNAL_PAGE_LEN
}

/// Validate the `--meta-node-kib` knob (§5.1): allowed values
/// 64/128/256/512/1024; sub-256 KiB settings return `Ok` with a warning
/// string naming the reduced per-volume record-value cap `node_size/4`
/// (§4.2) for the CLI to print. Anything else is a typed refusal —
/// including everything below the 64 KiB floor.
pub fn validate_node_kib(kib: u32) -> Result<(usize, Option<String>), KvError> {
    if ![64, 128, 256, 512, 1024].contains(&kib) {
        return Err(KvError::Corrupt(format!(
            "--meta-node-kib {kib} is not supported: allowed values are 64|128|256|512|1024 \
             (64 KiB floor, 1 MiB ceiling — design §5.1)"
        )));
    }
    let bytes = kib as usize * 1024;
    let warning = if kib < 256 {
        Some(format!(
            "--meta-node-kib {kib} trades xattr/layout headroom for cold-read latency: the \
             per-volume record-value cap becomes {} bytes (node_size/4, design §4.2) — values \
             above it are rejected and layout maps spill to the indirect mechanism sooner",
            record_value_cap(bytes)
        ))
    } else {
        None
    };
    Ok((bytes, warning))
}

/// Sector-0 classification for the mount/format version gate (§6.1) —
/// blank distinguished from garbage so the operator error is actionable.
#[derive(Debug, Clone)]
pub enum VolumeFormat {
    /// All-zero magic: never formatted. Callers decide loudness (mount:
    /// "run `squeezefs format` first"; format: proceed).
    Blank,
    /// SqueezeFS magic with version ≤ 2: the retired fixed-geometry
    /// format. **No longer mountable or readable** — mount and generation
    /// derivation refuse loud ("reformat required"); `format` treats it
    /// as an already-formatted volume (`--force` reformats it to v3).
    V2Legacy,
    /// A validated v3 superblock.
    V3(SuperblockV3),
}

/// Classify a sector-0 image (pure): [`VolumeFormat::Blank`] for zeroed
/// magic; loud errors for foreign magic, versions above 3 ("upgrade
/// squeezefs"), checksum mismatches, and v3 structural/feature-gate
/// failures. The version check precedes checksum verification — an
/// unknown version must be reported as such, never as a checksum
/// mismatch.
pub fn classify_sector0(sector: &[u8]) -> Result<VolumeFormat, KvError> {
    if sector.len() != SUPERBLOCK_V3_LEN {
        return Err(KvError::Corrupt(format!(
            "sector-0 image must be {SUPERBLOCK_V3_LEN} bytes, got {}",
            sector.len()
        )));
    }
    let magic = &sector[OFF_MAGIC..OFF_MAGIC + 8];
    if magic == [0u8; 8] {
        return Ok(VolumeFormat::Blank);
    }
    if magic != MAGIC_VALUE {
        return Err(KvError::Corrupt(format!(
            "invalid superblock magic {magic:?} (expected {MAGIC_VALUE:?}) — corrupted or \
             foreign volume"
        )));
    }
    // The version check precedes checksum verification: a future format
    // may change checksum semantics, so an unknown version must be
    // reported as such, never as a checksum mismatch.
    let version = u32::from_le_bytes(sector[OFF_VERSION..OFF_VERSION + 4].try_into().unwrap());
    match version {
        // Retired v2 format: recognized (SqueezeFS magic), never decoded —
        // there is no v2 reader left. The classification alone lets the
        // mount refuse with the precise "no longer supported" message and
        // lets `format --force` reformat the volume.
        v if v <= 2 => Ok(VolumeFormat::V2Legacy),
        SUPERBLOCK_V3_VERSION => Ok(VolumeFormat::V3(SuperblockV3::decode_sector(sector)?)),
        v => Err(KvError::Corrupt(format!(
            "unsupported metadata format version {v} (this binary supports <= \
             {SUPERBLOCK_V3_VERSION}) — upgrade squeezefs"
        ))),
    }
}

/// The DUR-5 generation stamped in a sector image (0 on any image an
/// older binary or `format` wrote). Only meaningful for images that
/// already passed [`classify_sector0`].
pub fn sector_generation(sector: &[u8]) -> u64 {
    if sector.len() < OFF_SB_GENERATION + 8 {
        return 0;
    }
    u64::from_le_bytes(
        sector[OFF_SB_GENERATION..OFF_SB_GENERATION + 8]
            .try_into()
            .expect("8 bytes"),
    )
}

/// Byte length of `path` — file size or block-device capacity (the
/// `seek(End)` form the CLI's `get_backing_device_size` uses, which is
/// correct for both). A control-plane probe at mount/format, never an
/// I/O path.
fn volume_len(path: &Path) -> Option<u64> {
    use std::io::Seek;
    if let Ok(mut f) = std::fs::File::open(path) {
        if let Ok(n) = f.seek(std::io::SeekFrom::End(0)) {
            if n > 0 {
                return Some(n);
            }
        }
    }
    std::fs::metadata(path)
        .ok()
        .map(|m| m.len())
        .filter(|n| *n > 0)
}

/// **DUR-5** — where the redundant superblock copy lives: the LAST
/// aligned sector of the volume.
///
/// *Why the tail and not a second sector next to sector 0:* every
/// existing v3 volume puts the root ledger at offset 4096 (`plan`), so
/// there is no reserved space beside sector 0 to A/B into — claiming one
/// would move the ledger, i.e. break the on-disk layout for every
/// formatted volume. The tail sector needs no layout change at all:
/// fresh formats reserve it out of the heap (one extent at most), and on
/// a volume whose heap already runs to the end the copy is simply not
/// written (honest degradation, logged once) rather than scribbled over
/// a live node.
pub fn backup_offset(volume_len: u64) -> Option<u64> {
    let sector = SUPERBLOCK_V3_LEN as u64;
    let aligned = (volume_len / sector) * sector;
    // Below two sectors there is no volume to speak of; the copy must
    // never alias sector 0.
    aligned.checked_sub(sector).filter(|off| *off >= sector)
}

/// The backup slot for `sb` on a `volume_len`-byte volume, or `None`
/// when the geometry leaves no room for it (a heap that runs to the end
/// — pre-DUR-5 formats).
fn backup_slot_for(sb: &SuperblockV3, volume_len: u64) -> Option<u64> {
    backup_offset(volume_len).filter(|off| *off >= sb.heap.end())
}

/// Read one 4 KiB sector, zero-extending a short read (a stub file
/// smaller than the sector classifies as Blank, which is what it is).
async fn read_sector(path: &Path, offset: u64) -> Result<Vec<u8>, KvError> {
    let got = crate::uring_fs::read_at(path, offset, SUPERBLOCK_V3_LEN).await?;
    let mut full = vec![0u8; SUPERBLOCK_V3_LEN];
    let n = got.len().min(SUPERBLOCK_V3_LEN);
    full[..n].copy_from_slice(&got[..n]);
    Ok(full)
}

/// Which copy answered a resolved superblock read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperblockSlot {
    Primary,
    Backup,
}

/// Read + classify sector 0 of `path` via `crate::uring_fs` (io_uring-
/// only, AGENTS.md), falling back to the DUR-5 redundant copy when the
/// primary is unusable. Never grows or mutates the volume.
pub async fn classify_volume(path: &Path) -> Result<VolumeFormat, KvError> {
    classify_volume_slot(path).await.map(|(fmt, _)| fmt)
}

/// [`classify_volume`] plus which copy answered — the input to the
/// mount-time self-heal ([`repair_primary_superblock`]).
pub async fn classify_volume_slot(path: &Path) -> Result<(VolumeFormat, SuperblockSlot), KvError> {
    let named = |e: KvError| match e {
        // Prefix classification failures with the volume path — these are
        // operator-facing mount/format refusals.
        KvError::Corrupt(msg) => KvError::Corrupt(format!("{}: {msg}", path.display())),
        other => other,
    };
    let primary = read_sector(path, 0).await?;
    let primary_res = classify_sector0(&primary);

    // The redundant copy is consulted ONLY when the primary cannot serve:
    // a readable primary is authoritative, so a stale or crafted tail
    // sector can never displace it.
    if let Ok(fmt) = &primary_res {
        if !matches!(fmt, VolumeFormat::V3(_)) {
            return primary_res
                .map(|f| (f, SuperblockSlot::Primary))
                .map_err(named);
        }
        return Ok((primary_res.map_err(named)?, SuperblockSlot::Primary));
    }
    let primary_err = primary_res.err().expect("checked above");

    let Some(len) = volume_len(path) else {
        return Err(named(primary_err));
    };
    let Some(off) = backup_offset(len) else {
        return Err(named(primary_err));
    };
    let backup = read_sector(path, off).await?;
    match classify_sector0(&backup) {
        // The recovered image must itself reserve the slot it was read
        // from: on a volume whose heap runs to the tail those bytes are a
        // live node, never a superblock.
        Ok(VolumeFormat::V3(sb)) if off >= sb.heap.end() => {
            log::error!(
                "{}: sector 0 is unreadable ({primary_err}) — mounting from the redundant \
                 superblock copy at offset {off} (generation {}). Sector 0 is repaired on \
                 the next write mount; investigate the device.",
                path.display(),
                sector_generation(&backup)
            );
            Ok((VolumeFormat::V3(sb), SuperblockSlot::Backup))
        }
        _ => Err(named(primary_err)),
    }
}

/// Write `sb` to `path`: the DUR-5 redundant copy FIRST (barriered), then
/// sector 0 (barriered). A tear on sector 0 therefore always recovers
/// FORWARD to this image, and a tear on the copy leaves the durable
/// primary untouched.
///
/// Both images carry `generation`; callers derive it with
/// [`next_superblock_generation`] so newest-valid-wins is decidable.
pub async fn write_superblock_v3(path: &Path, sb: &SuperblockV3) -> Result<(), KvError> {
    let generation = next_superblock_generation(path).await;
    write_superblock_at_generation(path, sb, generation).await
}

/// [`write_superblock_v3`] with an explicit generation.
async fn write_superblock_at_generation(
    path: &Path,
    sb: &SuperblockV3,
    generation: u64,
) -> Result<(), KvError> {
    let img = sb.encode_sector_at_generation(generation)?;
    match volume_len(path).and_then(|len| backup_slot_for(sb, len)) {
        Some(off) => {
            crate::uring_fs::write_at(path, off, img.clone()).await?;
            crate::uring_fs::fdatasync(path.to_path_buf()).await?;
        }
        None => log::warn!(
            "{}: no room for the redundant superblock copy (the heap runs to the end of \
             the volume — a pre-DUR-5 format); sector 0 has no backup. Reformatting \
             reserves the tail sector.",
            path.display()
        ),
    }
    crate::uring_fs::write_at(path, 0, img).await?;
    crate::uring_fs::fdatasync(path.to_path_buf()).await?;
    Ok(())
}

/// One past the highest generation either copy carries.
async fn next_superblock_generation(path: &Path) -> u64 {
    let mut newest = 0u64;
    if let Ok(sector) = read_sector(path, 0).await {
        if classify_sector0(&sector).is_ok() {
            newest = newest.max(sector_generation(&sector));
        }
    }
    if let Some(off) = volume_len(path).and_then(backup_offset) {
        if let Ok(sector) = read_sector(path, off).await {
            if classify_sector0(&sector).is_ok() {
                newest = newest.max(sector_generation(&sector));
            }
        }
    }
    newest + 1
}

/// Mount-time self-heal (DUR-5): if the volume mounted off the redundant
/// copy, rewrite sector 0 from it. Returns whether a repair was
/// performed. Write mounts only — a read-only probe must never write.
pub async fn repair_primary_superblock(path: &Path) -> Result<bool, KvError> {
    match classify_volume_slot(path).await? {
        (VolumeFormat::V3(sb), SuperblockSlot::Backup) => {
            let generation = next_superblock_generation(path).await;
            let img = sb.encode_sector_at_generation(generation)?;
            crate::uring_fs::write_at(path, 0, img).await?;
            crate::uring_fs::fdatasync(path.to_path_buf()).await?;
            log::error!(
                "{}: repaired a damaged sector 0 from the redundant superblock copy \
                 (generation {generation})",
                path.display()
            );
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Per-path serialization of the sector-0 read-modify-write (DUR-5).
/// Two concurrent stampers used to both read the old feature word and the
/// second write erased the first's bit — which defeats the whole point of
/// the bits, whose ordering law is "durable BEFORE the record it gates".
/// Cross-process exclusion is the D0 single-writer mount guard's job;
/// this closes the in-process race.
static SB_WRITE_LOCKS: once_cell::sync::Lazy<
    std::sync::Mutex<
        std::collections::HashMap<std::path::PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>,
    >,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn sb_write_lock(path: &Path) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    SB_WRITE_LOCKS
        .lock()
        .unwrap()
        .entry(path.to_path_buf())
        .or_default()
        .clone()
}

/// Stamp one incompat `bit` on `path`'s superblock (shared body of the
/// KD-14 bit setters). Returns whether the bit was NEWLY set (`false` =
/// already stamped, no write). Refuses blank / legacy-v2 / corrupt
/// volumes loud.
///
/// Sector 0 is written only here and at format, never by the live backend
/// (checkpoints flip the root ledger). The read-modify-write is
/// serialized per path (DUR-5) so concurrent stampers compose, and each
/// write lands on the redundant copy before sector 0.
async fn set_incompat_bit(path: &Path, bit: u64, what: &str) -> Result<bool, KvError> {
    let lock = sb_write_lock(path);
    let _held = lock.lock().await;
    match classify_volume(path).await? {
        VolumeFormat::V3(mut sb) => {
            if sb.features_incompat & bit != 0 {
                return Ok(false);
            }
            sb.features_incompat |= bit;
            write_superblock_v3(path, &sb).await?;
            Ok(true)
        }
        VolumeFormat::Blank => Err(KvError::Corrupt(format!(
            "{}: cannot stamp the {what} bit on an unformatted volume — run \
             `squeezefs format` first",
            path.display()
        ))),
        VolumeFormat::V2Legacy => Err(KvError::Corrupt(format!(
            "{}: format v2 is no longer supported; reformat required",
            path.display()
        ))),
    }
}

/// Stamp [`FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE`] on `path`'s superblock
/// — the "first non-trivial lifecycle commit" gate (KD-14). Callers must
/// invoke this **before** committing the durable record the bit gates
/// (bit-before-durable-record ordering, design-volume-lifecycle §7): a
/// crash between the bit write and the record commit leaves a set old
/// binaries refuse and this binary mounts unchanged — the safe prefix.
pub async fn set_volume_lifecycle_bit(path: &Path) -> Result<bool, KvError> {
    set_incompat_bit(
        path,
        FEATURE_INCOMPAT_KV_VOLUME_LIFECYCLE,
        "volume-lifecycle",
    )
    .await
}

/// Stamp [`FEATURE_INCOMPAT_KV_GUEST_SLOTS`] on `path`'s superblock —
/// step **(1)** of the §5.5.1a bit-before-first-stamp ordering
/// invariant. Callers must invoke this (and let the write land durably)
/// **before** the volume's first stamp-extended ledger slot is written;
/// see the constant's doc for why the order is load-bearing. Fresh
/// `--meta-slots` formats take the other path: the bit rides the planned
/// superblock, which format's flip discipline stamps LAST behind a
/// barrier over the already-written (stamped) ledger — sector 0 is
/// zeroed first, so no crash prefix is mountable by any binary at all.
pub async fn set_guest_slots_bit(path: &Path) -> Result<bool, KvError> {
    set_incompat_bit(path, FEATURE_INCOMPAT_KV_GUEST_SLOTS, "guest-slots").await
}

/// Stamp [`FEATURE_INCOMPAT_KV_LAYOUT_DELTAS`] on `path`'s superblock —
/// step **(1)** of the bit-before-first-delta-record ordering invariant
/// (see the constant's doc). Callers must invoke this (and let the
/// write land durably — the backend barriers with `sync_device`)
/// **before** the volume's first layout-delta journal entry is written.
pub async fn set_layout_deltas_bit(path: &Path) -> Result<bool, KvError> {
    set_incompat_bit(path, FEATURE_INCOMPAT_KV_LAYOUT_DELTAS, "layout-deltas").await
}

/// Stamp [`FEATURE_INCOMPAT_KV_SLOT_MIGRATION`] on `path`'s superblock —
/// step **(1)** of the VL5b bit-before-first-extended-record ordering
/// invariant (see the constant's doc). Callers must invoke this (and let
/// the write land durably) **before** the volume's first VL5b-extended
/// ledger slot or guest-keyspace record is written.
pub async fn set_slot_migration_bit(path: &Path) -> Result<bool, KvError> {
    set_incompat_bit(path, FEATURE_INCOMPAT_KV_SLOT_MIGRATION, "slot-migration").await
}

/// Stamp [`FEATURE_INCOMPAT_KV_DURABLE_TERM`] on `path`'s superblock —
/// the S2 upgrade path for a volume formatted before the durable writer
/// term existed (the batched Phase-8 reformat window; mount NEVER calls
/// this — see the constant's doc). Returns whether the bit was newly
/// set. The volume must be offline: the caller holds the D0 guard, and
/// the next mount's gate starts the era ladder at 1.
///
/// Ordering note: unlike bits 2/4/5 this bit gates no
/// silently-misdecoded record — a pre-S2 binary refuses the volume
/// outright, and the term record it would not understand is only
/// written by mounts that see the bit. Stamp-then-crash is therefore
/// inert: the volume mounts era-less on the old binary's refusal and
/// era-1 here.
pub async fn set_durable_term_bit(path: &Path) -> Result<bool, KvError> {
    set_incompat_bit(path, FEATURE_INCOMPAT_KV_DURABLE_TERM, "durable-term").await
}

/// Stamp [`FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS`] on `path`'s superblock —
/// the §6.2 item-1 upgrade path for a volume formatted before durable
/// block-reference accounting existed (the batched Phase-8 reformat
/// window; **mount NEVER calls this**). Returns whether the bit was newly
/// set. The volume must be offline (the caller holds the D0 guard).
///
/// Ordering note, same class as bit 7: the bit gates no
/// silently-misdecoded record. A pre-item-1 binary refuses a stamped
/// volume outright (the bit intersects no prior mask), and the
/// `TREE_BLOCK_REFS` root + records only ever come from a mount that saw
/// the bit — so stamp-then-crash is inert. The first mount after the
/// stamp mints the (empty) tree root and starts accounting from the
/// derived census, which is exactly the state the pre-stamp mount
/// rebuilt anyway.
pub async fn set_block_refcounts_bit(path: &Path) -> Result<bool, KvError> {
    set_incompat_bit(
        path,
        FEATURE_INCOMPAT_KV_BLOCK_REFCOUNTS,
        "durable-block-refcounts",
    )
    .await
}

/// Stamp [`FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING`] on `path`'s
/// superblock — the §6.2 items-8/10 upgrade path (the batched Phase-8
/// reformat window; **mount NEVER calls this**). Returns whether the bit
/// was newly set. The volume must be offline (the caller holds the D0
/// guard) AND every member of the set must be stamped before the next
/// mount, because scope engagement requires unanimity
/// ([`crate::writer_scope::resolve_scope_for_set`]) — a half-stamped set
/// simply mounts unscoped, which is safe but pointless.
///
/// Ordering note, same class as bits 7 and 9: the bit gates no
/// silently-misdecoded record. A pre-items-8/10 binary refuses a stamped
/// volume outright, and scoped keys / node-scoped markers only ever come
/// from a mount that saw the bit — so stamp-then-crash is inert. The
/// first mount after the stamp finds its staging roots bound to the
/// UN-scoped generation, adopts them (the `ScopeUpgrade` arm — durable
/// acked staged payloads are never discarded by the upgrade) and
/// re-stamps them node-scoped.
pub async fn set_writer_scoped_staging_bit(path: &Path) -> Result<bool, KvError> {
    set_incompat_bit(
        path,
        FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING,
        "writer-scoped-staging",
    )
    .await
}

/// Stamp [`FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA`] on `path`'s superblock
/// — the DLM **S7** capability gate (the Phase-8 batched reformat window;
/// **mount NEVER calls this**, ruling D9). Returns whether the bit was
/// newly set. The volume must be offline (the caller holds the D0 guard).
///
/// Ordering: the bit gates a MOUNT-TIME arming decision, not a record
/// decode, so stamp-then-crash is inert — the next mount either arms
/// multi-writer (if asked) or does not, and either way every structure is
/// byte-identical.
pub async fn set_multi_writer_data_bit(path: &Path) -> Result<bool, KvError> {
    set_incompat_bit(
        path,
        FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA,
        "multi-writer-data",
    )
    .await
}

/// Stamp [`FEATURE_INCOMPAT_KV_INO_LANES`] on `path`'s superblock — the
/// **Phase-8** upgrade path for spec §6.2 item 5 (per-writer ino lanes).
/// `Ok(false)` = already present. Caller holds the volume OFFLINE (the D0
/// guard); ruling D9 forbids mount and `plan` from doing this.
///
/// Stamp-then-crash is inert: the bit gates ino MINTING only, an older
/// binary refuses the volume outright, and a stamped volume whose next
/// mount is solo mints lane-0 inos — a subset of the dense space it would
/// have minted anyway, so no ino is ever reused either way.
pub async fn set_ino_lanes_bit(path: &Path) -> Result<bool, KvError> {
    set_incompat_bit(path, FEATURE_INCOMPAT_KV_INO_LANES, "ino-lanes").await
}

/// Stamp [`FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION`] on `path`'s
/// superblock — the **Phase-8** upgrade path for spec §6.2 item 6
/// (`offset ‖ incarnation` block keys). `Ok(false)` = already present.
/// Caller holds the volume OFFLINE (the D0 guard); ruling D9 forbids mount
/// and `plan` from doing this.
///
/// **Refuses loud on a volume without [`FEATURE_INCOMPAT_KV_DURABLE_TERM`]**
/// (bit 7): the incarnation's unrepeatability across a remount IS the
/// durable writer term, so stamping this onto a term-less volume would
/// restart the lifetime stamps at every mount — a detection that lies is
/// worse than no detection, because the read path would then serve a
/// matching stale key.
///
/// Mixed keys are expected and safe: blocks published before the stamp
/// carry bare keys (incarnation 0 = "no lifetime named"), and they only
/// ever become refusable once their offset is genuinely reallocated under
/// a stamped lifetime — which is exactly the dangerous case.
pub async fn set_block_key_incarnation_bit(path: &Path) -> Result<bool, KvError> {
    let VolumeFormat::V3(sb) = classify_volume(path).await? else {
        return Err(KvError::Corrupt(format!(
            "{}: not a v3 volume — cannot stamp block-key incarnations",
            path.display()
        )));
    };
    if sb.features_incompat & FEATURE_INCOMPAT_KV_DURABLE_TERM == 0 {
        return Err(KvError::Corrupt(format!(
            "{}: refusing to stamp block-key incarnations (bit 13) on a volume without the \
             durable writer term (bit 7) — the incarnation stamp is \
             `(writer_term << {}) | lane_seq`, so without a durable era it would restart at \
             every mount and a stale key would MATCH the offset's new lifetime (spec §6.2 \
             item 6 / §6.3). Stamp bit 7 first.",
            path.display(),
            crate::dlm::GRANT_SEQ_BITS,
        )));
    }
    set_incompat_bit(
        path,
        FEATURE_INCOMPAT_KV_BLOCK_KEY_INCARNATION,
        "block-key-incarnation",
    )
    .await
}
