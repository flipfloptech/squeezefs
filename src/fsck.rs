//! PR VL6a — **online report-only fsck** (design-volume-lifecycle §5.6,
//! KD-9/KD-17; repair is VL6b and consumes the findings this module
//! verifies).
//!
//! Check classes (C1–C13; C8–C13 postdate the VL6a seven):
//!
//! | Class | What | Source of truth |
//! |---|---|---|
//! | C1 | meta node integrity: checksum ride-along (node reads verify on
//! read) + checksum-valid semantic checks (key schema per tree, value
//! decode, in-page ordering) | v3 node checksums + record codecs |
//! | C2 | block_map ↔ allocator cross-check: **leaked** =
//! allocated-unreferenced, **lost** = referenced-unallocated /
//! out-of-range | the `df` census walk vs allocator state |
//! | C3 | refcount vs actual referencer count (clone-aware; the §5.4
//! step-2 mover pre-publish ledger consulted) | tree walk +
//! `refcount_core` reads |
//! | C4 | orphan `active_block:` / `active_block_ext:` staged custody
//! whose ino has no live meta | staged custody scan, read-only |
//! | C5 | staging-dir generation validity vs the mounted volume-set
//! generation | the generation-marker predicates, read-only |
//! | C6 | capacity accounting drift: used-blocks arithmetic vs the
//! tracked refcount population | allocator gauges |
//! | C7 | data scrub (KD-17, `--scrub` / `squeezefs scrub`): AEAD open on
//! encrypted volumes, frame decode on compressed (incl. the bit-31 raw
//! escape), readability-only on plain (`scrub_readability_only` is the
//! honesty gauge) | stored AEAD tags / frame structure / read status |
//! | C9 | **unreferenced inodes**: an inode record in `TREE_INODES` that
//! no dentry in `TREE_DENTRIES` names | one dentry-tree pass (the
//! referenced set) differenced against the census's live-inode set |
//! | C10 | **inode-plane reference consistency**: `nlink` vs the number
//! of distinct names referencing the inode (both directions), and
//! dentries naming an inode that does not exist | the SAME dentry pass's
//! name counts vs the census's `nlink`, and the reverse difference of the
//! same two sets |
//! | C11 | **map-plane consistency** (kvmap,
//! `docs/design-kvmap-block-map-tree.md` §3 fsck + A3): (a) orphan
//! tree-7 map records — the owner ino has no live inode record, or its
//! head is not kvmap-class (crossing residue); (b) a `kvmap:` head with
//! nonzero size and ZERO tree records (the fully-empty coverage case —
//! the size-vs-sparse ambiguity keeps partial coverage out of scope).
//! **REPORT-ONLY**: a false quarantine would hole a live crossing, and
//! the A1 residue sweep is the reclaim path. Zero-FP shields, BOTH
//! mandatory per A3: the in-flight crossing registry
//! (`crossing_in_flight` — an incomplete pass records no verdict for a
//! registered ino; C9's era floor is structurally inapplicable, since a
//! map record's ino may be years old while its crossing is live on THIS
//! mount) and the settle + re-check ladder under the ino's exclusive 4a
//! lease (the train holds 4a across sweep → chunks → flip, so a lease
//! held here brackets out any live train) | the tree-7 owner skip-scan
//! vs the inode records + layout heads; the census's kvmap extraction |
//! | C12 | **tenant-range consistency** (small-file packing,
//! `docs/design-small-file-packing.md` §5.9): two live mappings on one
//! block whose `[off, off + ceil(len))` windows intersect at DIFFERENT
//! `off` (a slot minted inside another slot — `C12Overlap`), or a
//! decorated mapping whose window reaches past the chunk, starts off the
//! LBA grain, or does not decode (`C12Overrun`). **REPORT-ONLY** (the C8
//! posture). | the census mapping list itself — one interval sort per
//! `(vol, offset, incarnation)`, no new walk |
//! | C13 | **orphan image extent** (design-symmetric-metadata §5.8.5,
//! symmetric PR 3): a heap extent an appender's grant holds CLAIMED that
//! no slot-tree root reaches and no pending-free names — the successor
//! image of a root swap the crash left unpublished, or a grant remainder
//! the page could not name. Repair = return to the bitmap through the
//! appender's own ring. Absent on every flat volume (no grant exists). |
//! the backend's census under its SMO + mint serialization
//! (`KvMetaBackend::c13_orphan_image_extents`) — one paged walk of the
//! interior population per volume, skipped when no grant holds a claim |
//!
//! ## C12 — tenant-range consistency (the packing class)
//!
//! The C8 ledger says HOW MANY references a block has; nothing said their
//! windows sit inside the block and nest legally. The packer mints slots
//! by one `fetch_add` on a block one mount owns, so it can never produce
//! two windows sharing a start, and the two LEGAL sharing classes both
//! keep the SAME `off`: the identical-window clone share (a promoted
//! source's clone carries its mapping verbatim) and that clone composed
//! with the passthrough clip (same `off`, shorter `len` — the shorter is a
//! prefix of the same image). Same-`off` can ONLY arise from clone + clip;
//! different-`off` intersection can ONLY arise from a defect — so the
//! corruption class and the legal classes are disjoint by construction,
//! and two whole-block referencers (both `[0, chunk)`) collapse like any
//! same-`off` pair.
//!
//! The decorated window is decoded by the class's OWN tolerant decoder,
//! never the read funnel's: `parse_block_mapping` refuses every violation
//! with `EIO` (the read path's contract), while the census resolves the
//! BASE through `clean_block_key` and counts the reference — so
//! `bk:garbage:len` is healthy for C2 and unreadable for every reader, and
//! without the Overrun arm fsck would never say so.
//!
//! **Zero false positives.** A finding is a SUSPECT first: the census read
//! the two layouts at different instants, and between them a block may
//! have been freed, recycled as a fresh pack and refilled, so the census's
//! "intersection" can name two lifetimes of one offset. The settle, then a
//! FRESH re-read of BOTH layouts under their inos' exclusive 4a leases
//! held together (one canonical acquisition; a layout publish takes the
//! same lease, so the pair is a consistent cut) must reproduce the
//! different-`off` intersection on the CURRENT mappings; a bit-13 volume
//! additionally keys the group on the incarnation the mappings name. An
//! OPEN pack block is exempt (its slots are being minted right now: the
//! tenants mid-flight ride the in-flight registry, the pack the pack-open
//! ledger), counted on `pack_ledger_exempted` / `inflight_exempted`.
//!
//! **Repair is REFUSED**: two tenants overlapping at different `off` means
//! at least one is wrong and nothing on the volume says which — quarantining
//! both would destroy the right one; an unreadable window's base block IS
//! referenced and restating the window would fabricate a mapping. The
//! counter `fsck_tenant_overlap_findings` is the live must-stay-0
//! tripwire; `fsck_repair_classC12` is structurally 0.
//!
//! ## C9 — unreferenced inodes (the class S3.5 left owed)
//!
//! `docs/design-cow-kv-metadata.md` §4.10a keeps cross-volume `create`
//! deliberately un-wrapped: its crash residue is an inode record with no
//! name for an op the caller was never told succeeded, and wrapping the
//! hottest cross-volume shape would tax every create for a claim nobody
//! can observe. "Owed instead: an fsck class for unreferenced inodes."
//! Three shapes reach it — (1) that crashed cross-volume create
//! (`nlink == 1`, no dentry, no blocks); (2) **pre-S3.5 field damage**,
//! where a filesystem that ran the old code and crashed mid cross-volume
//! `link`/`unlink` carries the same shape *plus every block the inode
//! owned* — S3.5 stops new occurrences and does nothing about existing
//! ones, so this class is the only way an operator learns a volume
//! carries such damage and its repair is the only cleanup path; (3)
//! future S9 causes (a dead writer's in-flight create; a recovered
//! cross-volume plan whose `MintInode` applied while its `InsertDentry`
//! volume was fenced).
//!
//! Nothing else sees them: `reclaim_orphaned_batch` admits only
//! `nlink == 0` **FORGET'd** inos and the kernel never learned this ino
//! exists, so nothing will ever FORGET it; C2/C3 ask "referenced by
//! nobody" about BLOCKS, and an unreferenced inode's layout still names
//! its blocks, so the allocator census agrees with the tree and every
//! block class stays (correctly) silent.
//!
//! **Walk direction.** A per-inode "does any dentry name me?" probe is
//! the unindexed reverse-dentry scan (POSIX-4's `meta_parent_scans`,
//! O(total dentries), legitimate only on the `open_by_handle_at`
//! reconnect path). C9 instead runs ONE sequential pass over
//! `TREE_DENTRIES` marking every dentry's TARGET ino into an
//! [`InoBitmap`], and differences it against the live-inode bitmap the
//! census pass marks as it already walks `TREE_INODES` — two bits per
//! inode (≈ 25 MiB at the ≥ 100 M-inode cap), no per-inode I/O, and the
//! per-suspect reads are bounded by real damage.
//!
//! **Zero false positives.** A live create legitimately holds an inode
//! record before its dentry, so a settle window alone would fire on
//! every in-flight create on a busy filesystem. The candidate filter is
//! therefore the **writer era's ino floor** (DLM S2's era; the §4.8
//! watermark captured per keyspace at open —
//! [`crate::meta_backend::kv::backend::KvMetaBackend::minted_in_prior_era`]):
//! inos are monotonic and never reused, so only records that survived a
//! PRIOR mount are candidates, and nothing this mount can mint is one.
//! The residue is by definition from a prior mount, so the filter costs
//! no coverage — with the honest consequence that residue created during
//! THIS mount is reported by the NEXT mount's scan, never this one.
//! Composed with the existing ladder: suspect → settle → a FRESH dentry
//! pass, re-checked under the ino's exclusive 4a lease (which is what
//! catches the one live way a name can appear for an unreachable inode —
//! an `open_by_handle_at` reconnect, or an S9 plan's late `InsertDentry`
//! — and clears it). The block-plane exemptions (allocation epoch,
//! in-flight allocation registry, mover ledger) do not apply: C9's
//! object is an inode, and the era floor is its structural equivalent.
//!
//! **Deliberately out of scope: the `nlink == 0` unreferenced shape.**
//! That is the POSIX unlinked-but-open state (and POSIX-15's
//! rename-overwrite crash orphan). Separating "prior-era inode unlinked
//! while still open in THIS mount" from "corpse nobody will ever FORGET"
//! needs the live open-count/reclaim registries, which this context does
//! not carry; the era floor alone cannot do it. Claiming it would trade
//! a leak for destroying an open file's data, so C9 does not claim it —
//! stated here rather than hidden.
//!
//! ## C10 — inode-plane reference consistency (C9's safety half)
//!
//! C9 answers *presence* ("is this inode named at all?"). Enumerating
//! what pre-S3.5 crash damage can also contain left two shapes with no
//! detector, and only one of them is a leak:
//!
//! 1. **`nlink` above the name count.** A cross-volume `link` whose count
//!    step committed and whose dentry step did not leaves `nlink == 2`
//!    with ONE name. C9 is correctly silent (a name exists), and nothing
//!    else looks: the inode and every block it owns can never be
//!    reclaimed — a permanent leak that reads as healthy.
//! 2. **`nlink` below the name count, `nlink == 0` with a live name, or a
//!    name resolving to nothing.** A cross-volume `unlink` whose count
//!    step committed and whose name step did not leaves a dentry
//!    resolving to an inode whose count no longer covers it. Once
//!    ordinary unlinks drive such a count to 0, **`destroy_inodes`'
//!    live-`nlink` skip stops protecting an inode a live path still
//!    resolves** — that direction is DATA LOSS, not a leak, and it is
//!    reachable on field volumes today. S3.5 (`24ef223c`) made such a
//!    dentry *removable*; nothing ever **found** one. This is the
//!    priority half of the class.
//!
//! **One walk, not two.** C9's dentry pass already visits every dentry;
//! C10's counts ride it. The set representation stays C9's — the cheap
//! shape is the existing "named at all" [`InoBitmap`] plus a **small map
//! for the inos named MORE THAN ONCE**, because `nlink > 1` is rare
//! (hardlinks are the exception in every real tree) and
//! [`InoBitmap::mark`] already *returns* whether the bit was newly set.
//! An ino absent from that map is named exactly once iff its bit is set,
//! so no per-ino counter is ever materialized. The census side is the
//! mirror: it already decodes every `InodeValue`, so it records the
//! **non-directory live inodes whose `nlink != 1`** — the hardlink
//! population plus damage, never the whole tree. Both sides therefore
//! cost ≈ 0 on a healthy volume and the candidate set is
//! `{nlink != 1} ∪ {named more than once}`, which is empty on a tree
//! without hardlinks.
//!
//! Names are counted as **distinct `(global parent, name)` pairs**, not
//! as records: a VL5b slot migration legitimately has a dentry record on
//! the source AND the target volume mid-copy, and global inos are stable
//! across it (the VL5a law), so the pair dedupes what raw records would
//! double-count.
//!
//! **The dangerous shapes need no counting at all.** `nlink == 0` with a
//! live name, and a dentry whose ino has no record, are both the
//! **reverse** difference of the two sets C9 already builds (`referenced`
//! minus `live` — the census skips `nlink == 0` records, so both shapes
//! land there), split by one fresh per-candidate record read. They are
//! therefore immune to the count map's budget: the class degrades only in
//! its leak direction, never in its loss direction.
//!
//! **Zero false positives — and NOT by C9's era floor.** C9's shield is
//! the writer era's ino floor; it cannot serve here, because a prior-era
//! inode can be legitimately hardlinked one microsecond ago. What holds
//! instead, composed with the settle window and the FRESH dentry pass:
//!
//! * **The record witness.** Every same-volume op that changes an inode's
//!   name count mutates that inode's record in the SAME transaction —
//!   `link`/`unlink` move `nlink`, `rename` stamps the moved inode's
//!   Δctime — so `(nlink, ctime)` read under the ino's exclusive 4a lease
//!   **before and after** the fresh pass is a witness: any change clears
//!   the suspect. This is what kills the pass's own read-over-time skew
//!   (two dentry pages read either side of one atomic rename look like 0
//!   or 2 names), because the skew can only come from a commit that
//!   landed *during* the pass, i.e. inside the bracket.
//! * **The open cross-volume intent exemption.** A multi-commit plan is
//!   exactly the window where the count and the names legitimately
//!   disagree, and every such plan leaves a durable `SQZXTX01` intent
//!   until it retires (`xv_scan_intents`, one bounded range per volume,
//!   empty on a healthy set). Every ino a plan's steps name is exempt —
//!   the in-flight-registry role the block plane fills with
//!   `inflight_contains`.
//! * **Directories are never a count finding.** A directory's `nlink` is
//!   `2 + subdirectories` because it counts `.` and every child's `..`,
//!   which are synthesized and never records. Comparing it to "names in
//!   the dentry tree" would fire on every directory in the filesystem.
//!   (A directory named TWICE — an illegal shape — is consequently not
//!   claimed either; stated rather than hidden.)
//! * **Completeness gates.** An incomplete dentry pass ⇒ no verdict (C9's
//!   law); a census walk that could not finish ⇒ no verdict for the
//!   reverse arms (a missing live inode would make its names look
//!   dangling); a count map at its budget ⇒ no verdict for the count arms
//!   (a dropped entry would invert a comparison).
//!
//! The residual is stated rather than hidden: a *cross-volume* rename
//! stamps its moved inode's ctime as a separate per-volume fragment, so a
//! rename whose fragment lands after the post-witness could survive one
//! bracket. It must then survive a SECOND independent full pass with the
//! same skew, and — for the only repair that lowers a count — a THIRD at
//! repair time. A continuously rewritten file bumps its own ctime and so
//! keeps clearing: like C9's era floor, that costs coverage (the next run
//! reports it), never safety.
//!
//! **Interaction with the block plane.** A wrong `nlink` does not corrupt
//! block accounting — an inode with `nlink >= 1` is walked by the census
//! either way, so C2/C3/C8 stay silent alongside a count finding. The one
//! exception is structural and drives the repair order: the census
//! **skips `nlink == 0` records**, so a zero-count-with-a-name inode's
//! blocks read as allocated-unreferenced (C2-leaked). Raising the count
//! re-attaches them, so C10 runs BEFORE the block classes and the
//! repair's census is re-walked after an inode-plane raise — otherwise
//! the leaked-block free would destroy the data the raise just restored.
//!
//! **The POSIX-15 line.** C10 claims `nlink == 0` **with** a name (no
//! legitimate state has it). It does not claim `nlink == 0` **without**
//! one: that is POSIX unlinked-but-open and POSIX-15's rename-overwrite
//! orphan, which needs the live open-count/reclaim registries this
//! context does not carry — the same line C9 draws, from the other side.
//!
//! **Verify-before-report (KD-9)** — detection never mutates, and a
//! violation becomes a finding only after it survives the class's full
//! machinery:
//!
//! * Per-object classes (C1/C4/C5): suspect → settle window → final
//!   re-check under the object's DLM lease (lattice 2/4a — brief,
//!   per-object, never a global freeze).
//! * Cross-object allocator classes (C2/C3, and C6's aggregate): the
//!   **allocation-epoch filter** — a scan-latched side map fed by
//!   `allocate_block` only while a scan is armed (never the loom-verified
//!   incarnation seqlock) — whose two-epoch survival only **escalates**
//!   to the **in-flight allocation registry** liveness check. Age alone
//!   is never a verdict (retry-forever writeback and R5 parks
//!   legitimately span epochs). The final escalation's NORMATIVE order:
//!   **registry-absence first, then re-verify the reference state** — the
//!   registry contract (owners deregister only after their publish is
//!   durable AND visible to the reads fsck performs) makes both
//!   interleavings of a racing publish/deregister safe
//!   (reference-state-first would false-positive on a healthy
//!   just-published block).
//! * C7 failures re-verify online under a validated pin
//!   (`pin_block_validated`): a mapping that moved, or an in-flight
//!   patch (unstable incarnation), clears the suspect instead of
//!   reporting a torn read.
//!
//! **Online / offline duality (§5.8)**: online reads the live daemon's
//! RAM-authoritative state (arc-swap snapshots) with the suspects
//! machinery; offline (`--offline`, read-only probe opens) needs no
//! suspects — nothing is in flight by definition. **Offline sharding**
//! (`--shards k/N`) walks only the k-th ino-residue shard with zero
//! coordination; `merge-reports` unions the JSON outputs (repeated
//! per-shard classes dedupe by identity; census counters sum). **C9's
//! shard rule**: a shard covering a subset of INODES still needs the
//! full referenced set for those inodes, so it filters dentries by the
//! child ino their VALUE carries (`child_ino % N == k`) — never by the
//! dentry key, whose parent ino says nothing about which inode is
//! named. Every shard therefore walks the whole dentry tree but marks
//! only its own residue: sharding divides the bitmap and the inode
//! pass, not the dentry walk, and each inode is judged by exactly one
//! shard (no double counting, nothing missed). Honesty
//! notes: offline C2-*leaked* and C3 have no durable allocator ground
//! truth (data-volume allocator state is mount-session RAM, rebuilt from
//! the same walk), so offline detects the *lost*/out-of-range arm plus
//! C1/C4/C5 — stated here rather than faked.
//!
//! The C1–C6 scan is coordinator-local by design (§5.1.6 division: it
//! reads live RAM-authoritative state no remote client can see). C7 is
//! designed distributable; v1 runs it on the coordinator-local fabric
//! worker — the §5.1.6 wire dispatches whole jobs only (`Noop`), so
//! shipping scrub sub-shards over the wire's read-shard seam is a
//! follow-up, recorded honestly in `JobType::wire_executable`.

use crate::block_allocator::BlockAllocator;
use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use crate::meta_backend::{Metadata, RoutedMetaBackend};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Settle-window override (ms) for the online suspect machinery.
pub const FSCK_SETTLE_MS_ENV: &str = "SQUEEZEFS_FSCK_SETTLE_MS";
/// Default settle window (§5.6 step 2).
pub const FSCK_SETTLE_DEFAULT_MS: u64 = 2_000;
/// Report schema version.
pub const FSCK_REPORT_SCHEMA: u32 = 1;

/// Census page size (records per tree-range fetch) — also the throttle
/// duty-cycle unit for the scan.
const SCAN_PAGE: usize = 512;
/// Scrub throttle batch (blocks per duty-cycle unit).
const SCRUB_BATCH: usize = 8;
/// Staged-custody scan bound (staging dirs are thousands of files).
const CUSTODY_SCAN_MAX: usize = 1_000_000;

// ---------------------------------------------------------------------------
// Options / report types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsckMode {
    /// Live coordinator: suspects → settle → re-check under leases;
    /// epoch filter + registry escalation armed.
    Online,
    /// Read-only probe posture: nothing is in flight by definition — no
    /// suspects machinery, violations report directly.
    Offline,
}

#[derive(Clone)]
pub struct FsckOptions {
    pub mode: FsckMode,
    /// Add the C7 data scrub to the run.
    pub scrub: bool,
    /// C7 only (`squeezefs scrub`).
    pub scrub_only: bool,
    /// KD-3 duty-cycle percentage (0/≥100 = unthrottled).
    pub throttle_pct: u32,
    /// Offline zero-coordination sharding: scan only inos with
    /// `ino % n == k` (`(k, n)`, `k < n`).
    pub shard: Option<(u32, u32)>,
    /// The §5.6 settle window (online).
    pub settle: Duration,
    /// Cooperative cancellation (fabric cancel).
    pub cancel: Arc<AtomicBool>,
    /// KD-MW-16 (design-mw-fleet-jobs §3): scan the staging dirs FULL
    /// (unfiltered by the ino-residue shard). Fleet shards set this —
    /// staging is per-mount, so each member covers its OWN custody
    /// records completely and the union is exactly-once by disjointness,
    /// where a residue-filtered scan would leave every member's
    /// foreign-residue keys covered by NOBODY.
    pub staging_full: bool,
    /// KD-MW-16: the caller (the fleet fan-out — `run_fleet` is the one
    /// production setter) already holds the allocator scan latch for the
    /// whole fleet window — do not arm or release it here (a nested
    /// release would drain the C2/C3 epoch side map mid-fleet). Public
    /// only because `FsckOptions` is constructed by struct-update in the
    /// contracts; never set this outside a held-latch scope.
    pub assume_latched: bool,
    /// Evaluate the INODE-PLANE classes (C9/C10) in this run. `true`
    /// everywhere except a fleet MEMBER's shard (KD-MW-16), where the
    /// plane is a **one-view plane**: its verdicts are
    /// census-vs-dentry-pass AGREEMENT, so both walks and every
    /// verification read must share one authority's coherent instant. A
    /// fleet member's shard runs over its S5 staleness-bounded reader
    /// view, whose per-volume checkpoint projections sit at DIFFERENT
    /// instants mid-churn — a dentry read at volume A's older instant
    /// beside its record at volume B's newer one manufactures exactly
    /// the `C10ZeroNlinkNamed`/`C10DanglingDentry` loss shapes from a
    /// healthy tree, and the member-side ladder re-reads the same
    /// shifted view, so verification cannot clear it (the 2026-08-17
    /// tarx conviction: 25 findings, all self-healed once the reader's
    /// poll caught up).
    ///
    /// **The skip is POSTURE-conditional since KD-PV-16**
    /// (`docs/design-per-volume-claim-admission.md` §5.8.1/§5.8.2 F5):
    /// an **OWNER** shard evaluates the plane over [`Self::owned_volumes`]
    /// while a **member/reader** shard still skips it. The distinction is
    /// one of KIND, not of degree — an owner reads records it APPENDS to
    /// (authoritative, not projected) plus a cross-owner reference set
    /// the §5.9.2 freeze law makes unchangeable, so the one-view law is
    /// restated as *one coherent view per OWNER over its OWN inos*
    /// rather than weakened. Without the restatement, KD-PV-7's scoping
    /// composed with KD-PV-14's single coordinator would leave 1/K of
    /// the set's inodes unevaluated online (R17), and PR 6's
    /// `fsck_findings == 0` gate would pass trivially.
    pub inode_plane: bool,
    /// KD-PV-16: run the inode plane and NOTHING else — the owner
    /// shard's shape. The block plane's census residue is the
    /// coordinator's own shard set (`shard`); a plane shard that also
    /// reported a census would double-count it at the merge, and the
    /// layout extraction the census pays for is work this shard has no
    /// use for.
    pub inode_plane_only: bool,
    /// **KD-PV-7**: the volume indices this node APPENDS to — the
    /// inode-plane CANDIDATE scope. `None` = every volume, which is
    /// every mount that has not armed a multi-owner plane (the shipped
    /// posture, byte-identical).
    ///
    /// Scoping the candidates is not a weakening: it is the "residue
    /// from the current mount is reported by the next one" law one axis
    /// over — *residue on a volume this node does not append to is
    /// reported by that volume's owner* — because C9's zero-FP shield is
    /// the writer era's ino floor, and a peer's floor is a snapshot of a
    /// cursor another node advances.
    ///
    /// **The REFERENCED set stays whole-set** (§5.8.0): a name living on
    /// a peer's volume can reference an ino this owner is responsible
    /// for, and KD-PV-15's subtree roots are exactly that population.
    /// Scoping both halves reports `C9Unreferenced` for every subtree
    /// root on the supported deployment.
    pub owned_volumes: Option<Vec<usize>>,
    /// `true` ⇔ a multi-owner plane is armed on this mount (§5.9.3):
    /// detection is unchanged, the destructive/dangerous repair trio
    /// goes report-only, and the freeze precondition is checked before
    /// the plane records any verdict.
    pub multi_owner: bool,
}

impl FsckOptions {
    fn settle_from_env() -> Duration {
        let ms = std::env::var(FSCK_SETTLE_MS_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(FSCK_SETTLE_DEFAULT_MS);
        Duration::from_millis(ms)
    }

    pub fn online() -> Self {
        Self {
            mode: FsckMode::Online,
            scrub: false,
            scrub_only: false,
            throttle_pct: 100,
            shard: None,
            settle: Self::settle_from_env(),
            cancel: Arc::new(AtomicBool::new(false)),
            staging_full: false,
            assume_latched: false,
            inode_plane: true,
            inode_plane_only: false,
            owned_volumes: None,
            multi_owner: false,
        }
    }

    pub fn offline() -> Self {
        Self {
            mode: FsckMode::Offline,
            ..Self::online()
        }
    }

    /// Does this pass judge inodes homed on `v_idx`? Unscoped (`None`) =
    /// every volume — the shipped single-authority answer.
    fn owns_volume(&self, v_idx: usize) -> bool {
        match &self.owned_volumes {
            None => true,
            Some(owned) => owned.contains(&v_idx),
        }
    }

    /// The volumes this pass's inode plane covers, ascending — the
    /// KD-PV-16 coverage assertion's local half.
    fn covered_volumes(&self, volume_count: usize) -> Vec<usize> {
        match &self.owned_volumes {
            None => (0..volume_count).collect(),
            Some(owned) => {
                let mut v: Vec<usize> = owned
                    .iter()
                    .copied()
                    .filter(|v| *v < volume_count)
                    .collect();
                v.sort_unstable();
                v.dedup();
                v
            }
        }
    }
}

/// One verified violation: full identity (class, object, evidence) —
/// the input VL6b's repair planner consumes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FsckFinding {
    /// `"C1"`..`"C12"`.
    pub class: String,
    /// The object's identity (ino / key / volume+offset / path).
    pub object: String,
    /// What was observed (human-readable, machine-greppable).
    pub evidence: String,
    /// PR VL6b (§5.6a): the structured identity the repair engine acts
    /// on. `None` on reports produced by older binaries — such findings
    /// are refused by repair (re-run detection), never guessed at from
    /// the display strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<FindingId>,
}

/// PR VL6b (§5.6a): machine identity of a verified finding — everything
/// the per-class repair action needs, carried IN the report (findings
/// are the only repair input; repair re-verifies each is still current
/// before acting).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingId {
    /// C1: checksum-bad / torn node (walk could not advance). `slot`
    /// names a forest volume's slot tree (its raw walk; `tree` is 0).
    C1Torn {
        vol: usize,
        tree: u8,
        #[serde(default)]
        slot: Option<crate::meta_backend::kv::record::ForestSlot>,
        cursor_hex: String,
    },
    /// C1: checksum-valid semantic damage on one record.
    C1Semantic {
        vol: usize,
        tree: u8,
        key_hex: String,
    },
    /// C1: a forest slot-tree record under a key the codec refuses
    /// (checksum-valid; the raw stored key).
    C1RawKey {
        vol: usize,
        slot: crate::meta_backend::kv::record::ForestSlot,
        key_hex: String,
    },
    /// C2: allocated (tracked) with zero referencers.
    C2Leaked { vol: String, offset: u64 },
    /// C2: referenced but unallocated / out-of-range / unresolvable.
    /// `unrepairable_shape` = out-of-range / unaligned / unknown-backend
    /// mappings whose allocator can never legally be repaired — the
    /// content-verify arm is skipped and the action is quarantine.
    C2Lost {
        vol: String,
        offset: u64,
        ino: u64,
        block_idx: u32,
        mapping: String,
        unrepairable_shape: bool,
    },
    /// C3: refcount ≠ counted references.
    C3Refcount { vol: String, offset: u64 },
    /// C4: orphan staged custody.
    C4Orphan { dir: PathBuf, key: String, ino: u64 },
    /// C5: stale/invalid staging generation.
    C5Staging { dir: PathBuf },
    /// C6: capacity-accounting drift.
    C6Drift { vol: String },
    /// C8: **durable-vs-derived block-reference drift** (pre-RC spec §6.2
    /// item 1): the durable reference population for a block disagrees
    /// with the layout walk's census. Must never fire on a healthy
    /// volume — the ledger and the layouts that justify it ride ONE
    /// checksummed journal entry.
    C8DurableRefDrift { vol: String, offset: u64 },
    /// C7: scrub-failed block.
    C7Scrub {
        ino: u64,
        block_idx: u32,
        mapping: String,
    },
    /// C9: **unreferenced inode** — an inode record no dentry names (the
    /// class `docs/design-cow-kv-metadata.md` §4.10a owed, and the only
    /// detector of pre-S3.5 cross-volume damage). The global ino IS the
    /// identity; everything else repair needs it re-derives under the
    /// ino's lease at verify time.
    C9Unreferenced { ino: u64 },
    /// C10: `nlink` **above** the number of distinct names — the LEAK
    /// direction (the inode and its blocks can never be reclaimed).
    C10NlinkTooHigh { ino: u64 },
    /// C10: `nlink` **below** the number of distinct names — the
    /// DANGEROUS direction (the count no longer covers a live path, so
    /// ordinary unlinks can make a reachable inode reclaimable).
    C10NlinkTooLow { ino: u64 },
    /// C10: `nlink == 0` while dentries still name the inode —
    /// unambiguous damage (no legitimate state has it) and the shape
    /// `destroy_inodes`' live-`nlink` skip no longer protects.
    C10ZeroNlinkNamed { ino: u64 },
    /// C10: a dentry whose child ino has **no inode record** — the name
    /// resolves to nothing. Identity is the RECORD (volume + exact key),
    /// not the name: a collision chain holds several keys for one name.
    C10DanglingDentry {
        vol: usize,
        key_hex: String,
        child_ino: u64,
    },
    /// C11 (kvmap map plane, design-kvmap-block-map-tree §3 fsck + A3):
    /// orphan tree-7 map records — the owner ino has no live inode
    /// record, or its head is not kvmap-class (crossing residue). `ino`
    /// is the volume-LOCAL owner ino (the identity tree-7 records key
    /// on). REPORT-ONLY: repair refuses (a false quarantine would hole a
    /// live crossing; the A1 residue sweep reclaims).
    C11OrphanMapRecords { vol: usize, ino: u64, records: u64 },
    /// C11: a `kvmap:` head with nonzero size and ZERO tree records —
    /// the fully-empty coverage-mismatch case (the size-vs-sparse
    /// ambiguity keeps partial coverage undetectable). REPORT-ONLY.
    C11EmptyKvmapHead { vol: usize, ino: u64 },
    /// C11 (PR 6a, design §12): run-vs-point coverage sanity — a point
    /// record on a DIFFERENT volume strictly inside a covering run's
    /// span. Same-volume points inside a run are legal by the §2 read
    /// law (the arithmetic-equal shadow and the overwrite class alike);
    /// a cross-volume override is rare enough to warrant eyes.
    /// REPORT-ONLY.
    C11RunForeignShadow {
        vol: usize,
        ino: u64,
        run_start: u32,
        idx: u32,
    },
    /// C12 (design-small-file-packing §5.9): two live tenant windows on
    /// one `(vol, offset)` intersect at DIFFERENT `off` — a slot minted
    /// inside another slot. The two referencers are the identity (the
    /// block alone would collapse every pair on it into one). REPORT-ONLY:
    /// at least one tenant is wrong and nothing on the volume says which.
    C12Overlap {
        vol: String,
        offset: u64,
        ino_a: u64,
        block_idx_a: u32,
        ino_b: u64,
        block_idx_b: u32,
    },
    /// C12: a decorated mapping whose window breaks the size-carrying
    /// form's law — `off + ceil(len) > CHUNK_SIZE`, `off % LBA_GRAIN != 0`,
    /// or a decoration present but undecodable. The census resolves its
    /// BASE and counts the reference (C2 is silent); only a read refuses
    /// it, so this arm is the only report of it. REPORT-ONLY.
    C12Overrun {
        vol: String,
        offset: u64,
        ino: u64,
        block_idx: u32,
        mapping: String,
    },
    /// C13 (design-symmetric-metadata §5.8.5): an **orphan image extent**
    /// — a heap extent of meta volume `vol` that `appender`'s grant holds
    /// claimed, reached by no slot-tree root and parked by no pending-free
    /// (the unpublished root swap's successor, the page's truncated
    /// unclaimed remainder). Repair: the appender frees it in its own ring
    /// and the cadence returns it to the bitmap.
    C13OrphanImageExtent {
        vol: usize,
        appender: u32,
        extent: u64,
    },
    /// C16 (design-symmetric-metadata §5.8.5, PR 7): **shared-index
    /// drift** — a SHARED-flagged reference the index home does not name
    /// (source or target ino), or an index entry whose ino holds no SHARED
    /// reference. Report-only, the C8 posture: the flag and the index are
    /// two durable homes of one fact, and restating one from the other
    /// would erase the evidence of which side lied.
    C16SharedIndexDrift {
        vol_tag: u64,
        block_idx: u64,
        owner_ino: u64,
        block_index: u32,
        /// `true` = a flag without an entry; `false` = an entry without a
        /// flag.
        flag_side: bool,
    },
    /// C17 (design-symmetric-metadata §5.6.5 / §5.8.5, PR 7b): **stripe
    /// consistency** — a stripe ino named by more than one directory's
    /// map, a map naming a missing stripe (or an incomplete map under a
    /// commit marker), a dentry in stripe `i` whose `hash % K ≠ i`, or a
    /// name left in the directory's own tree after `migrating` cleared.
    /// Report-only (the C8 posture); `fsck_stripe_findings` must stay 0.
    C17StripeInconsistency {
        /// The striped directory (0 when the shape names a stripe alone).
        dir: u64,
        /// The stripe involved (0 for a directory-only shape).
        stripe: u64,
        /// One of `multiply-mapped` / `missing-stripe` / `incomplete-map`
        /// / `misrouted-name` / `unmigrated-name`.
        shape: String,
        /// The name involved, when the shape has one.
        name: String,
    },

    /// C14 (design-symmetric-metadata §5.8.5, PR 10): **slot custody
    /// conflict** — forest slot `slot` of meta volume `vol` attested
    /// `Live` on the pages of two appenders at tree 0's generation, or
    /// `Live` on `appender_a`'s page while tree 0 leases it to
    /// `appender_b` at that generation. No legal schedule writes it (a
    /// grant moves tree 0 before the new lessee's page names the slot,
    /// and a release clears the page before tree 0 unleases); the mount
    /// REFUSES on it and the remedy is the operator's attestation,
    /// `squeezefs appender clear`. Report-only here.
    C14SlotCustodyConflict {
        vol: usize,
        slot: u32,
        appender_a: u32,
        appender_b: u32,
    },
    /// C15 (design-symmetric-metadata §5.8.5, PR 10): **un-recovered
    /// appender** — a `Live` or `Recovering` page of meta volume `vol`
    /// whose identity the death ledger (volume 0's `dead_member:`
    /// records) names, with acked records still in its ring window
    /// (`window_entries`) or slots still leased to it. Repair (online,
    /// on the volume's manager) = the §5.9 recovery run; the mount path
    /// runs the same recovery before it serves.
    C15UnrecoveredAppender {
        vol: usize,
        appender: u32,
        node_token: u64,
        mount_slot: u32,
        window_entries: u64,
    },
}

/// The §10 `fsck_*` / `scrub_*` counter families, per run (the process
/// gauges in [`crate::fuse_client::METRICS`] accumulate the same names).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct FsckCounters {
    pub inodes_scanned: u64,
    /// Tree pages walked (one per leaf-range fetch — the C1 walk unit).
    pub nodes_walked: u64,
    /// C9: dentry records whose target ino the referenced-set pass
    /// indexed (the cheap-direction pass's engagement gauge — 0 on a run
    /// that never built the set).
    pub dentry_refs_indexed: u64,
    /// C9: live inodes with no dentry that the **writer-era ino floor**
    /// exempted — i.e. records THIS mount minted, the in-flight-create
    /// shape. The live-create false-positive shield's engagement gauge
    /// (the block plane's `epoch_exempted` analogue); nonzero under
    /// concurrent creates, and its growth is the proof the shield is
    /// doing work rather than sitting vacuous.
    pub current_era_exempted: u64,
    /// C9: unnamed prior-era candidates an OPEN cross-volume plan NAMES
    /// (PR 13e review round 2, Issue 10 — the inode plane's in-flight
    /// exemption for C9, C10's `open_intent_inos` read by the evaluate AND
    /// the confirm): a cross-owner create commits the child's record before
    /// its `InsertDentry` ships, and a ship the holder refuses leaves the
    /// intent OPEN for the roll-forward cadence; once the child's slot
    /// reads UNLEASED (its lessee released or left) tree 0 makes the record
    /// a prior-era candidate with no name — the roll-forward's object, never
    /// C9's, until the intent retires. 0 on a healthy set; growth = plans
    /// left open at census time (read beside `xv_cross_owner_intents_open`
    /// / `_stuck`).
    pub unreferenced_intent_exempted: u64,
    /// C10: inos named MORE THAN ONCE that the name counting tracked — the
    /// counting extension's engagement gauge (0 on a tree with no hardlinks
    /// and no damage, which is why the class costs nothing there).
    pub nlink_names_counted: u64,
    /// C10: verified findings per direction. `high` is the LEAK direction;
    /// `low` and `zero_named` are the DATA-LOSS direction (an operator
    /// stops and reads on those two), and `dangling` counts names that
    /// resolve to nothing.
    pub nlink_mismatch_high: u64,
    pub nlink_mismatch_low: u64,
    pub nlink_zero_named: u64,
    pub dangling_dentries: u64,
    /// C10 suspects cleared by the class's own guards — the record witness
    /// `(nlink, ctime)` bracketing the fresh dentry pass, the open
    /// cross-volume intent exemption, and the census/pass concurrency
    /// re-read. The zero-FP shield's engagement gauge (the inode plane's
    /// `inflight_exempted`): nonzero under concurrent link/unlink/rename
    /// traffic, and its growth is the proof the shield is not vacuous.
    pub nlink_transient_cleared: u64,
    /// **KD-PV-16's coverage assertion**
    /// (`docs/design-per-volume-claim-admission.md` §5.8.1): how many of
    /// the set's volumes this pass's inode plane actually judged. A
    /// completed pass must satisfy `== volume_count`; a short count makes
    /// the pass INCOMPLETE rather than silently narrowing to 1/K, and it
    /// is what stops `fsck_findings == 0` from passing trivially on a
    /// fleet. `0` on a pass that recorded no verdict.
    pub inode_plane_volumes_covered: u64,
    /// **KD-PV-7 engagement**: inode-plane candidates left to their own
    /// volume's owner (this node does not append there, so its era floor
    /// is a snapshot of a cursor another node advances). The division of
    /// labour, counted rather than silent; 0 on every unscoped pass.
    pub inode_plane_foreign_scoped: u64,
    /// PR 8 (design-symmetric-metadata §5.5 "Maintenance coordinator" —
    /// the LESSEE shards): inode-plane candidates left to the LESSEE of
    /// their slot (this mount neither leases the slot nor, as the volume's
    /// manager, finds it unleased). 0 on every unarmed pass.
    pub inode_plane_foreign_slot_scoped: u64,
    /// Symmetric PR 10 (review round 2, Issue 12): C9/C10 candidates a
    /// FOREIGN appender's un-replayed ring window names — in flight at
    /// their holder, judged by the pass after its checkpoint or recovery.
    pub inode_plane_window_scoped: u64,
    /// Symmetric PR 12b (review round 1, Issue 1): the DENTRY pass met a
    /// slot a LIVE foreign appender leases — its dentries live in the
    /// lessee's tree, which this mount holds only as a projection whose
    /// staleness the census cannot bound (the `sym-storm` leg read a
    /// remounted joiner's 1,444 removals as 442 dangling names) — so the
    /// inode plane recorded NO verdict this run. One per volume so met.
    pub inode_plane_foreign_dentry_scoped: u64,
    /// Symmetric PR 12b rounds 4/5: slot trees the raw C1 walk SKIPPED as
    /// PROJECTIONS (`fsck_c1_projection_slots_scoped` —
    /// `SlotCoverage::unjudged_slots`): at the manager a LIVE foreign
    /// lessee's, at a member every tree not its own — their images are
    /// appended into and re-rooted past this mount's root word, so a raw
    /// walk reads a routing loop over a healthy tree (the `sym-storm`
    /// leg's false `C1Torn`). The pass is INCOMPLETE over them.
    pub c1_projection_slots_scoped: u64,
    /// PR 8: `fsck_inode_plane_slots_covered` — Σ over the pass's volumes
    /// of the slots this mount's inode plane judged: the slots it LEASES
    /// plus, on the volume's manager, the UNLEASED slots (tree 0's) —
    /// `≡ leased ∪ unleased` by construction; every hosted slot on an
    /// unarmed volume.
    pub inode_plane_slots_covered: u64,
    /// The population whose verdict is **undecidable online under
    /// multi-owner**: a dangling name whose dentry lives on a peer's
    /// volume while its child ino homes here. Neither owner may decide
    /// it — this node cannot commit the name's removal and cannot read
    /// the peer's dentry tree as anything but a projection, and the peer
    /// cannot read this ino's record as anything but one — so it is
    /// DECLINED, never guessed at, and the offline whole-set pass is its
    /// only detector. Stated here rather than left invisible.
    pub inode_plane_cross_owner_declined: u64,
    /// §5.8.2's engagement instrument, counted per PROPOSAL: owner
    /// shards whose inode-plane report the coordinator ADMITTED. **Must
    /// be > 0 on any K ≥ 2 fleet pass** — coverage that closes without an
    /// admitted proposal came from nowhere. (Per proposal, not per
    /// finding: a healthy fleet has no findings, and the question this
    /// gauge answers is whether the owners actually reported.)
    pub inode_plane_proposals_admitted: u64,
    /// §5.8.2's tripwire, counted per PROPOSAL: shard reports that had
    /// inode-plane findings DROPPED because the coordinator's own lease
    /// table + `OwnerMap` do not say the proposer owns the volume those
    /// findings are about. **Must stay 0 on a homogeneous fleet**; growth
    /// means a non-owner is proposing the plane, i.e. the
    /// `fix/mw-xv-unlink-c10` mirage path is live and this predicate is
    /// the only thing holding it.
    pub inode_plane_proposals_stripped: u64,
    pub blocks_checked: u64,
    pub refcounts_checked: u64,
    pub suspects: u64,
    pub suspects_cleared: u64,
    pub epoch_exempted: u64,
    pub inflight_exempted: u64,
    pub mover_ledger_exempted: u64,
    /// C2/C3 suspects excused by the small-file packer's pack-open ledger
    /// (design-small-file-packing §5.3): the +1 PIN of an OPEN pack block —
    /// exactly one per open pack, so a quiet mount reads `= open packs`;
    /// the tenants' transient references ride `inflight_exempted`.
    pub pack_ledger_exempted: u64,
    /// DLM S9 (rung-10 finding #5): block-plane verdicts DECLINED because
    /// the object lives in an allocation lane this mount does not own —
    /// a live peer writer's blocks are durably referenced and
    /// structurally untracked by this mount's own-lane census, so
    /// adjudicating them here is a category error (C2's foreign-lane
    /// exemptions + C6's whole-census decline under an engaged
    /// partition). The cross-writer oracle stays C8; the lane-aligned
    /// fleet-parallel fsck is rung 10c's (KD-MW-16).
    pub foreign_lane_exempted: u64,
    /// Symmetric PR 8/10 — C6's bitmap oracle on a grant-armed allocator:
    /// SET bits no reference, no open grant and no in-flight registration
    /// names — a dead incarnation's window remainder the next (re-)hold
    /// releases (`data_alloc_bitmap_leaks_released`). Informational; the
    /// LOSS direction is the C6 finding.
    pub alloc_bitmap_leak_candidates: u64,
    /// Symmetric PR 13 (defect 27): C2-lost verdicts DECLINED because the
    /// referenced offset, untracked in this mount's RAM refcount map, is
    /// SET in the allocation bitmap this mount HOLDS — a former lessee's
    /// mint (a slot released, handed over or recovered to this mount);
    /// the bitmap is the allocation truth on a grant-armed allocator.
    pub alloc_bitmap_tracked_exempted: u64,
    /// C11: verified orphan tree-7 map RECORDS (per record, not per owner
    /// — the census the design's must-stay-0 `fsck_map_orphan_records`
    /// gauge accumulates). 0 on healthy volumes.
    pub map_orphan_records: u64,
    /// C11: verified fully-empty `kvmap:` heads (nonzero size, zero tree
    /// records). 0 on healthy volumes.
    pub map_empty_heads: u64,
    /// C11 (PR 6a): verified cross-volume point records strictly inside
    /// a covering run's span (design §12's run-vs-point sanity arm).
    /// Same-volume shadows are legal (the §2 read law) and never counted.
    pub map_run_foreign_shadows: u64,
    /// C11's zero-FP registry shield engagement (design A3): verdicts
    /// withheld because the ino's crossing train is registered in flight
    /// — the map plane's `inflight_exempted`. Growth under live crossings
    /// is the proof the shield is not vacuous.
    pub crossing_exempted: u64,
    /// C12 (design-small-file-packing §5.9): verified tenant-range
    /// findings — both arms (two windows on one block intersecting at
    /// DIFFERENT `off`; a decorated window past the chunk, off the LBA
    /// grain, or undecodable). **Must stay 0** on healthy volumes: the
    /// live `fsck_tenant_overlap_findings` tripwire.
    pub tenant_overlap_findings: u64,
    /// C16 (design-symmetric-metadata §5.8.5, PR 7): confirmed shared-
    /// index drift findings — report-only; the live
    /// `fsck_shared_index_drift` gauge. 0 on every healthy set.
    pub shared_index_drift: u64,
    /// C17 (design-symmetric-metadata §5.6.5, PR 7b): confirmed stripe
    /// inconsistencies — report-only; the live `fsck_stripe_findings`
    /// gauge. 0 on every healthy striped tree.
    pub stripe_findings: u64,
    /// C14 (design-symmetric-metadata §5.8.5, PR 10): confirmed slot
    /// custody conflicts — report-only, the mount refuses on them; the
    /// live `fsck_slot_custody_conflicts` gauge. **Must stay 0.**
    pub slot_custody_conflicts: u64,
    /// C15 (design-symmetric-metadata §5.8.5, PR 10): confirmed
    /// un-recovered appenders — a ledgered death whose ring window the
    /// §5.9 recovery has not yet replayed; the live
    /// `fsck_unrecovered_appenders` gauge. 0 once every manager's poll
    /// has acted (the mount path recovers before serving).
    pub unrecovered_appenders: u64,
    pub findings: u64,
    pub scan_secs: u64,
    pub scrub_blocks_scanned: u64,
    pub scrub_bytes_scanned: u64,
    pub scrub_aead_verified: u64,
    pub scrub_frame_verified: u64,
    pub scrub_readability_only: u64,
    pub scrub_failures: u64,
}

/// **C9's ino set**: one bit per inode, dense in the space inos are
/// actually allocated in.
///
/// A global ino decomposes to `(slot, raw local)` through
/// [`crate::meta_backend::route_ino_width`] over the durable routing
/// width — the SAME decomposition on both sides (a dentry's target ino
/// and an inode record's own ino), derived from the global ino alone, so
/// the index is independent of the live slot→volume map and a slot
/// migration mid-scan cannot shift a bit. Raw locals are monotonic from
/// 2 per keyspace (§4.8) and never reused, so each keyspace's bits are
/// dense: **1 bit per inode ever allocated** — ≈ 12.5 MiB per set at the
/// ≥ 100 M-inode cap, against 4.8–6.4 GB for a `HashSet<u64>` of the same
/// population. Only populated slots allocate a word vector (a 65536-slot
/// width costs nothing for the slots a volume never minted in). The
/// house precedent is the allocator's A/B extent bitmap.
///
/// `raw < 2` never indexes: raw local 1 is the root pin / a per-keyspace
/// control record, which no dentry names by construction — the root
/// exemption falls out of the encoding instead of being a special case.
#[derive(Debug)]
pub struct InoBitmap {
    width: u64,
    raw_ceiling: u64,
    byte_budget: u64,
    bytes: u64,
    truncated: bool,
    slots: HashMap<u64, Vec<u64>>,
    marked: u64,
}

impl InoBitmap {
    /// Empty set over the volume set's durable routing `width`.
    ///
    /// `raw_ceiling` is the exclusive raw-local ino ceiling
    /// ([`crate::meta_backend::kv::backend::KvMetaBackend::max_local_ino_watermark`]
    /// folded over the set): no inode record can exist at or above it, so
    /// an ino there is ignored — **a dentry record's `child_ino` is never
    /// an allocation authority** (a corrupt value naming `u64::MAX` would
    /// otherwise size a bit vector from it: the `kv_bset` `record_count`
    /// lesson). `byte_budget` bounds the bit vectors in total; exceeding
    /// it sets [`Self::truncated`], and a truncated set makes C9 record
    /// **no verdict** rather than report from a partial one.
    pub fn new(width: u64, raw_ceiling: u64, byte_budget: u64) -> Self {
        Self {
            width,
            raw_ceiling,
            byte_budget,
            bytes: 0,
            truncated: false,
            slots: HashMap::new(),
            marked: 0,
        }
    }

    fn index_of(&self, global_ino: u64) -> Option<(u64, u64)> {
        // `route_ino_width` is defined from ino 1 up (its `ino - 2`
        // arithmetic underflows below that), and inos 0/1 index nothing
        // here anyway: 0 is not an ino at all (a corrupt dentry value can
        // carry it) and 1 is the root pin, which no dentry names.
        if global_ino < 2 {
            return None;
        }
        let (slot, raw) = crate::meta_backend::route_ino_width(global_ino, self.width);
        (raw >= 2 && raw < self.raw_ceiling).then(|| (slot, raw - 2))
    }

    /// Set `global_ino`'s bit; `true` when it was not already set.
    pub fn mark(&mut self, global_ino: u64) -> bool {
        let Some((slot, bit)) = self.index_of(global_ino) else {
            return false;
        };
        let w = (bit / 64) as usize;
        let have = self.slots.get(&slot).map(|v| v.len()).unwrap_or(0);
        if have <= w {
            let grow = ((w + 1 - have) * 8) as u64;
            if self.bytes + grow > self.byte_budget {
                self.truncated = true;
                return false;
            }
            self.bytes += grow;
            self.slots.entry(slot).or_default().resize(w + 1, 0);
        }
        let words = self.slots.get_mut(&slot).expect("resized above");
        let mask = 1u64 << (bit % 64);
        if words[w] & mask != 0 {
            return false;
        }
        words[w] |= mask;
        self.marked += 1;
        true
    }

    /// `true` ⇔ a mark was refused because the bit vectors reached their
    /// budget. The set is INCOMPLETE and C9 must record no verdict from
    /// it (the same posture as a dentry pass that could not finish).
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn contains(&self, global_ino: u64) -> bool {
        let Some((slot, bit)) = self.index_of(global_ino) else {
            return false;
        };
        self.slots
            .get(&slot)
            .and_then(|w| w.get((bit / 64) as usize))
            .is_some_and(|word| word & (1u64 << (bit % 64)) != 0)
    }

    /// Distinct inos marked.
    pub fn marked(&self) -> u64 {
        self.marked
    }

    /// Bit-vector bytes held (the RAM-cost gauge the ≥ 100 M-inode cap is
    /// stated against; excludes the per-slot map overhead).
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Every ino in `self` whose bit is clear in `other` — the C9
    /// difference (live inodes that no dentry names). Visits in
    /// unspecified order; allocates nothing.
    ///
    /// Word-parallel: one `AND NOT` per 64 inodes and one per-slot lookup
    /// per word vector, so a healthy volume's whole difference is a linear
    /// scan of `population / 64` words with NO per-inode work — the
    /// per-inode cost is paid only for inodes that really are unnamed.
    /// Both sets must come from the same routing width (they do: the
    /// width is the volume set's durable one).
    pub fn each_absent_from(&self, other: &Self, mut f: impl FnMut(u64)) {
        debug_assert_eq!(
            self.width, other.width,
            "difference across different routing widths would compare unrelated bits"
        );
        for (&slot, words) in &self.slots {
            let theirs = other.slots.get(&slot);
            for (w, &word) in words.iter().enumerate() {
                let mut absent = word & !theirs.and_then(|t| t.get(w)).copied().unwrap_or(0);
                while absent != 0 {
                    let b = absent.trailing_zeros() as u64;
                    absent &= absent - 1;
                    let raw = (w as u64) * 64 + b + 2;
                    f(crate::meta_backend::make_global_ino_width(
                        raw, slot, self.width,
                    ));
                }
            }
        }
    }
}

/// Per-shard partial census (cross-shard classes finalize at merge).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PartialCensus {
    /// Canonical volume id → offset → reference count from THIS shard's
    /// ino-residue walk.
    pub refs: HashMap<String, HashMap<u64, u32>>,
    /// KD-MW-16 (design-mw-fleet-jobs §4): this shard's referenced-
    /// mapping identities — what the fleet coordinator's FINALIZE needs
    /// to run the allocator classes over the merged census.
    /// `#[serde(default)]`: pre-10c reports stay decodable (they merge
    /// with no mappings and `mappings_complete = true`, which the
    /// offline `merge-reports` CLI never consults).
    #[serde(default)]
    pub mappings: Vec<MappingRef>,
    /// `false` ⇔ the mapping list was DROPPED (the oversize wire
    /// degrade) — the fleet coordinator then walks its own census for
    /// the allocator classes instead (loud, the stated Amdahl term).
    #[serde(default = "default_true")]
    pub mappings_complete: bool,
}

fn default_true() -> bool {
    true
}

/// The structured report (`--json` prints it verbatim; `merge-reports`
/// unions shard reports).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FsckReport {
    pub schema: u32,
    pub mode: String,
    /// `"k/N"` on sharded runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard: Option<String>,
    pub findings: Vec<FsckFinding>,
    pub counters: FsckCounters,
    /// Present on sharded runs (merge input).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial: Option<PartialCensus>,
    /// PR VL6b (§5.6a): the repair plan/outcome when the run was invoked
    /// with `--repair` (dry run) or `--repair --apply`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairReport>,
    /// **KD-PV-16**: the volume indices whose inode plane this run
    /// judged — the identities behind
    /// [`FsckCounters::inode_plane_volumes_covered`], carried so a
    /// coordinator can UNION them across owner shards. Empty when the
    /// plane recorded no verdict.
    ///
    /// A shard's declaration can only ever NARROW what the coordinator
    /// credits it with: the admission predicate intersects this list
    /// with the volumes the **coordinator's own** `OwnerMap` says that
    /// worker owns (§5.8.2 clause 2 — never the shard's own claim).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inode_plane_covered: Vec<usize>,
    /// Findings the ADMIN-LANE view elided to fit the wire cap
    /// ([`Self::to_bounded_json`]) — 0 on every durable/offline report,
    /// which is why it never serializes there. Truncation is COUNTED,
    /// never silent: the CLI names what the view did not show and the
    /// durable `job:{id}:report` record stays complete.
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub findings_elided: u64,
}

fn u64_is_zero(v: &u64) -> bool {
    *v == 0
}

impl FsckReport {
    pub fn has_findings(&self) -> bool {
        // Elided findings ARE findings: a bounded view must keep the
        // CLI's exit-1 verdict even for the part it did not print.
        !self.findings.is_empty() || self.findings_elided > 0
    }

    /// Render this report as JSON **bounded to `max_bytes` by
    /// construction** — the admin-lane view (the PR 8 campaign's "reply
    /// too large" wart): counters and every other field travel intact,
    /// the finding LIST is truncated to the longest prefix that fits,
    /// and [`Self::findings_elided`] carries the exact count dropped.
    ///
    /// Deliberate bounding is what keeps the wire cap's
    /// refuse-never-truncate law intact for everything else: an
    /// oversize reply still refuses unless it was bounded HERE, where
    /// the truncation is counted and the caller is told.
    ///
    /// `None` when even the finding-free skeleton exceeds `max_bytes`
    /// (a pathological caller bound — the admin cap is 60 KiB and the
    /// skeleton is O(counters)), so the caller can fall through to the
    /// loud refusal rather than serve a lie.
    pub fn to_bounded_json(&self, max_bytes: usize) -> Option<String> {
        let full = serde_json::to_string(self).ok()?;
        if full.len() <= max_bytes {
            return Some(full);
        }
        // Binary-search the longest fitting prefix. Encoding is
        // monotone in the prefix length (findings only ever add bytes),
        // so the search is sound; O(log n) encodes of a ≤ 60 KiB body.
        let mut view = self.clone();
        let (mut lo, mut hi) = (0usize, self.findings.len());
        let mut best: Option<String> = None;
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            view.findings = self.findings[..mid].to_vec();
            view.findings_elided = self.findings_elided + (self.findings.len() - mid) as u64;
            match serde_json::to_string(&view) {
                Ok(body) if body.len() <= max_bytes => {
                    best = Some(body);
                    lo = mid;
                }
                _ => hi = mid - 1,
            }
        }
        if best.is_none() && lo == 0 {
            // No finding fits — serve the counted skeleton if IT fits.
            view.findings = Vec::new();
            view.findings_elided = self.findings_elided + self.findings.len() as u64;
            let body = serde_json::to_string(&view).ok()?;
            if body.len() <= max_bytes {
                best = Some(body);
            }
        }
        best
    }
}

/// What the engine needs. Online: the live mount's meta + router.
/// Offline: probe opens + a probe-shaped router.
pub struct FsckCtx {
    pub meta: Arc<RoutedMetaBackend>,
    pub router: crate::routing::DataRouter,
    pub staging_dirs: Vec<PathBuf>,
    /// The mounted volume-set generation (C5 ground truth); `None`
    /// skips C5's generation arm.
    pub expected_generation: Option<String>,
}

/// The STAGING generation of an OPEN set (the
/// [`crate::meta_backend::volume_set_generation`] string computed from
/// the live superblocks, volume order preserved) — decorated with this
/// node's writer scope when every member carries incompat bit 10 (§6.2
/// item 10), because that is what the staging markers this value is
/// compared against actually hold. Byte-identical to the bare set
/// generation on every un-stamped set.
pub fn volume_generation(meta: &RoutedMetaBackend) -> String {
    use std::fmt::Write as _;
    let mut parts = Vec::with_capacity(meta.volumes.len());
    for kv in &meta.volumes {
        let mut s = String::with_capacity(3 + 32);
        s.push_str("v3:");
        for b in kv.superblock().uuid {
            let _ = write!(s, "{b:02x}");
        }
        parts.push(s);
    }
    let set = parts.join("|");
    let scope = crate::writer_scope::scope_for_features(
        meta.volumes
            .iter()
            .map(|kv| kv.superblock().features_incompat),
    );
    crate::writer_scope::staging_generation(&set, scope)
}

// ---------------------------------------------------------------------------
// Test hook: the §5.6 registry TOCTOU barrier
// ---------------------------------------------------------------------------

/// Invoked with the suspect's clean block key immediately BEFORE the
/// final registry-absence check of an escalated C2/C3 suspect — the
/// deterministic window the publish/deregister race test needs. `None`
/// in production; a set hook may block the engine (tests park it
/// deliberately).
static PRE_REGISTRY_CHECK_HOOK: parking_lot::RwLock<Option<Arc<dyn Fn(&str) + Send + Sync>>> =
    parking_lot::RwLock::new(None);

pub fn set_pre_registry_check_hook(hook: Arc<dyn Fn(&str) + Send + Sync>) {
    *PRE_REGISTRY_CHECK_HOOK.write() = Some(hook);
}

pub fn clear_pre_registry_check_hook() {
    *PRE_REGISTRY_CHECK_HOOK.write() = None;
}

fn fire_pre_registry_hook(key: &str) {
    let hook = PRE_REGISTRY_CHECK_HOOK.read().clone();
    if let Some(h) = hook {
        h(key);
    }
}

/// C7 read-fault injector (the G-VL-5(b) "dm-error or equivalent" arm
/// at the cargo tier: the rig's root legs can interpose a real error
/// target; in-process tests inject at the read seam). Called with the
/// device read offset; `true` = fail this read.
static SCRUB_READ_FAULT_HOOK: parking_lot::RwLock<Option<Arc<dyn Fn(u64) -> bool + Send + Sync>>> =
    parking_lot::RwLock::new(None);

pub fn set_scrub_read_fault_hook(hook: Arc<dyn Fn(u64) -> bool + Send + Sync>) {
    *SCRUB_READ_FAULT_HOOK.write() = Some(hook);
}

pub fn clear_scrub_read_fault_hook() {
    *SCRUB_READ_FAULT_HOOK.write() = None;
}

// ---------------------------------------------------------------------------
// Internal census structures
// ---------------------------------------------------------------------------

/// One referenced mapping (scrub + lost-check unit) — since KD-MW-16
/// also the fleet shard reports' mapping-identity record
/// (`PartialCensus::mappings`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MappingRef {
    pub ino: u64,
    pub block_idx: u32,
    /// The mapping string VERBATIM (decoration included).
    pub mapping: String,
    /// The CANONICAL volume id + offset the clean base key resolves to
    /// (`"?"`/0 for unresolvable mappings). Referencer matching must key
    /// on BOTH — offsets alias across volumes (a bare default-slot
    /// mapping carries the same offset number as an unrelated `oss2://`
    /// block; the leg-13 drain-concurrent FP root cause).
    pub vol: String,
    pub offset: u64,
    /// §5.6a quarantined (`damaged:`) mapping: counted for refcount
    /// coherence (the physical block is intentionally preserved), but
    /// never scrubbed and never a lost finding — it IS the repair.
    pub damaged: bool,
}

struct CensusOut {
    /// Canonical volume id → offset → reference count.
    refs: HashMap<String, HashMap<u64, u32>>,
    /// Every referenced mapping (C7 / lost checks).
    mappings: Vec<MappingRef>,
    /// Mappings that do not resolve to any known backend (C2 lost).
    unresolvable: Vec<MappingRef>,
    /// C9: every LIVE inode (`nlink > 0`) the walk visited, as bits — the
    /// set the referenced-ino set is differenced against. Free to carry:
    /// the census already walks `TREE_INODES`.
    live: InoBitmap,
    /// C10: global ino → `nlink`, for **non-directory** live inodes whose
    /// `nlink != 1`. The count arms' small side: the hardlink population
    /// plus damage, never the whole tree (directories are excluded because
    /// their `nlink` counts `.` and every child's `..`, which are
    /// synthesized and never records — module header). Free to carry: the
    /// census already decodes every `InodeValue`.
    odd_nlink: HashMap<u64, u32>,
    /// `false` ⇔ [`Self::odd_nlink`] hit its derived entry budget, so a
    /// missing entry would read as `nlink == 1` and invert a comparison:
    /// the C10 count arms then record NO verdict.
    odd_nlink_complete: bool,
    /// `false` ⇔ the inode walk could not finish (unreadable node —
    /// C1's business to report — or cancellation). A partial live set
    /// makes named inodes look like they have no record, so the C10
    /// REVERSE arms (dangling names, `nlink == 0` with a name) record no
    /// verdict. C9's direction is unaffected: a missing live inode is
    /// simply one fewer candidate.
    complete: bool,
    /// C11 (b) NOMINATIONS: `kvmap:` heads the census saw with nonzero
    /// size and ZERO tree-7 records, as `(vol, local ino, size)` — free
    /// to carry (the census already pages the kvmap extraction per
    /// layout). The verify ladder owns every verdict; sweep-cursor heads
    /// and corpses never nominate (A2: a truncate/unlink sweep
    /// legitimately leaves this shape in range).
    kvmap_empty_heads: Vec<(usize, u64, u64)>,
    inodes_scanned: u64,
}

/// An empty census — the single-layout probe shape (`census_layout` into a
/// scratch [`CensusOut`]). Its `live` set is inert by construction:
/// probes never mark inodes, so they pay nothing for the C9 machinery.
fn probe_census() -> CensusOut {
    CensusOut {
        refs: HashMap::new(),
        mappings: Vec::new(),
        unresolvable: Vec::new(),
        live: InoBitmap::new(1, 0, 0),
        odd_nlink: HashMap::new(),
        odd_nlink_complete: true,
        complete: true,
        kvmap_empty_heads: Vec::new(),
        inodes_scanned: 0,
    }
}

/// A C9 ino set sized for this volume set: the routing width, the ino
/// ceiling nothing can exist at or above (so no record's or dentry's ino
/// is an allocation authority), and the byte budget below.
fn ino_bitmap(meta: &RoutedMetaBackend) -> InoBitmap {
    let ceiling = meta
        .volumes
        .iter()
        .map(|kv| kv.max_local_ino_watermark())
        .max()
        .unwrap_or(crate::meta_backend::kv::ino_lane::LOCAL_INO_BASE);
    InoBitmap::new(meta.routing_width(), ceiling, ino_set_byte_budget())
}

/// Per-set bit-vector budget for the C9 ino sets — **derived**, never a
/// tuning constant: 1/64th of the R5 memory budget (a scan holds two such
/// sets, so ≈ 3 % of the budget transiently), floored at the design cap's
/// own requirement — the ≥ 100 M-inode cap
/// (`docs/design-cow-kv-metadata.md` §4.2) needs 100 M/8 = 12.5 MB of
/// bits, and 16 MiB is that with headroom for the sparse tail. A set that
/// hits the budget marks itself truncated and C9 records no verdict.
///
/// `pub` so the derivation carries a drift-is-red tie test (the house law
/// for every derived default) — see `tests/fsck_c9_tests.rs`.
pub fn ino_set_byte_budget() -> u64 {
    const DESIGN_CAP_FLOOR: u64 = 16 * 1024 * 1024;
    (crate::mem_budget::MEM_BUDGET.budget_bytes() / 64).max(DESIGN_CAP_FLOOR)
}

/// Charged bytes per C10 count-map entry — a `(parent, name)` pair or an
/// `(ino, nlink)` pair with its hash-table overhead. Not a tuning knob:
/// it is the accounting factor that converts [`ino_set_byte_budget`] into
/// an entry count, so the count maps ride the SAME byte budget as ONE C9
/// ino set (short names round down, a 255-byte name rounds up — the
/// budget is a bound, not a promise).
const C10_COUNT_ENTRY_BYTES: u64 = 64;

/// Entry budget for C10's count maps — **derived** from the ino-set byte
/// budget, never a tuning constant: the multi-name identities and the
/// `nlink != 1` map together may hold as many entries as one ino set holds
/// bytes worth. Exceeding it is not a failure — it makes the count arms
/// record **no verdict** (a dropped entry would invert a comparison),
/// while C10's DANGEROUS arms keep working because they read only the
/// bitmaps.
///
/// `pub` so the derivation carries a drift-is-red tie test (the house law
/// for every derived default) — see `tests/fsck_c10_tests.rs`.
pub fn c10_count_entry_budget() -> u64 {
    ino_set_byte_budget() / C10_COUNT_ENTRY_BYTES
}

/// One volume's allocator handle under its canonical id (device access
/// rides the router's key-resolved read path).
struct VolAlloc {
    id: String,
    alloc: Arc<BlockAllocator>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SuspectKind {
    /// C1: a specific record violating its tree schema.
    C1Record {
        vol: usize,
        tree: u8,
        key: Vec<u8>,
        why: String,
    },
    /// C1: a tree walk failed (checksum / undecodable node). `slot` names
    /// the slot tree of a forest volume's raw walk (`tree` is then the
    /// slot trees' header id, 0).
    C1Walk {
        vol: usize,
        tree: u8,
        slot: Option<crate::meta_backend::kv::record::ForestSlot>,
        cursor: Vec<u8>,
        error: String,
    },
    /// C1: a slot-tree record whose KEY the forest codec refuses (a kind
    /// byte that is another tree's id, a wrong length for its kind) — the
    /// kind-routed reads skip it; only the raw walk can name it.
    C1RawKey {
        vol: usize,
        slot: crate::meta_backend::kv::record::ForestSlot,
        key: Vec<u8>,
        why: String,
    },
    /// C2 leaked: allocated (tracked) with zero referencers.
    C2Leaked { vol: String, offset: u64 },
    /// C2 lost: referenced but unallocated / out-of-range / unknown
    /// backend. Carries the referencing mapping's identity for the
    /// finding (§5.6a repair input).
    C2Lost {
        vol: String,
        offset: u64,
        ino: u64,
        block_idx: u32,
        mapping: String,
        why: String,
    },
    /// C3: refcount ≠ counted references.
    C3Refcount {
        vol: String,
        offset: u64,
        expected: u32,
        actual: u32,
    },
    /// C8: durable-vs-derived block-reference drift (spec §6.2 item 1).
    C8DurableRefDrift {
        vol: String,
        offset: u64,
        durable: u32,
        derived: u32,
    },
    /// C4: staged custody without live ino meta.
    C4Orphan { dir: PathBuf, key: String, ino: u64 },
    /// C5: staging generation invalid.
    C5Generation { dir: PathBuf, why: String },
    /// C6: used-blocks accounting vs tracked population drift.
    C6Drift {
        vol: String,
        used: u64,
        tracked: u64,
    },
    /// C9: a live inode record, minted in a PRIOR writer era, that the
    /// dentry pass found no name for. The counted evidence rides along so
    /// the report states what was observed (a `blocks > 0` shape is the
    /// pre-S3.5 damage signature; `blocks == 0` is the crashed
    /// cross-volume create).
    C9Unreferenced {
        ino: u64,
        nlink: u32,
        size: u64,
        blocks: usize,
    },
    /// C10: `nlink` disagrees with the number of distinct names. ONE
    /// suspect kind for both directions — the direction is a property of
    /// the verified numbers, decided at verdict time, so neither arm can
    /// drift from the other's ladder. `names >= 1` always (a named-by-
    /// nobody inode is C9's object); `nlink >= 1` (the `nlink == 0` shape
    /// is its own kind below).
    C10NlinkMismatch { ino: u64, nlink: u32, names: u32 },
    /// C10: `nlink == 0` while `names >= 1` dentries still name the ino.
    C10ZeroNlinkNamed { ino: u64, names: u32 },
    /// C10: a dentry record naming an ino with no inode record.
    C10Dangling {
        vol: usize,
        key: Vec<u8>,
        /// Global parent ino (display only — `key` is the identity).
        parent: u64,
        name: String,
        child_ino: u64,
        /// `DT_*` from the dentry value: a `DT_DIR` dangling name cannot
        /// be removed without also deciding the parent's directory
        /// `nlink`, which this class does not verify — so repair refuses
        /// it rather than guessing.
        file_type: u8,
    },
    /// C11 (a): tree-7 records whose owner ino has no live inode record
    /// or whose head is not kvmap-class (crossing residue). `local_ino`
    /// is the volume-LOCAL owner ino the records key on.
    C11OrphanMapRecords { vol: usize, local_ino: u64 },
    /// C11 (b): a `kvmap:` head with nonzero size and ZERO tree records.
    C11EmptyKvmapHead {
        vol: usize,
        local_ino: u64,
        size: u64,
    },
    /// C11 (c) — PR 6a: a point record on a DIFFERENT volume strictly
    /// inside a covering run's span (design §12).
    C11RunForeignShadow {
        vol: usize,
        local_ino: u64,
        run_start: u32,
        idx: u32,
    },
    /// C12: two referencers of one block whose windows intersect at
    /// DIFFERENT `off` (design-small-file-packing §5.9). `a` is the
    /// window that reached furthest before `b` started inside it.
    C12Overlap {
        vol: String,
        offset: u64,
        a: TenantWindow,
        b: TenantWindow,
    },
    /// C12: a decorated mapping whose window violates the law or does not
    /// decode (`why` names the arm).
    C12Overrun {
        vol: String,
        offset: u64,
        ino: u64,
        block_idx: u32,
        mapping: String,
        why: String,
    },
    /// C13 (design-symmetric-metadata §5.8.5): a heap extent an
    /// appender's grant holds CLAIMED that no tree root of meta volume
    /// `vol` reaches (the census ran under the volume's SMO + mint
    /// serialization, so a live in-window image is never nominated).
    C13OrphanImageExtent {
        vol: usize,
        appender: u32,
        extent: u64,
    },
    /// C16: shared-index drift (design-symmetric-metadata §5.8.5, PR 7).
    C16SharedIndexDrift {
        vol_tag: u64,
        block_idx: u64,
        owner_ino: u64,
        block_index: u32,
        flag_side: bool,
    },
    /// C17: stripe consistency (design-symmetric-metadata §5.6.5, PR 7b).
    C17StripeInconsistency(C17Shape),
    /// C14: slot custody conflict (design-symmetric-metadata §5.8.5,
    /// PR 10) — the backend's custody census over the appender directory
    /// against tree 0.
    C14SlotCustodyConflict {
        vol: usize,
        slot: u32,
        appender_a: u32,
        appender_b: u32,
    },
    /// C15: un-recovered appender (design-symmetric-metadata §5.8.5,
    /// PR 10) — a ledgered death with a ring window or leased slots.
    C15UnrecoveredAppender {
        vol: usize,
        appender: u32,
        node_token: u64,
        mount_slot: u32,
        window_entries: u64,
    },
}

/// One referencer's device window on its block, as C12 judges it:
/// `[off, end)` in bytes within the chunk (a bare whole-block key is
/// `[0, CHUNK_SIZE)`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct TenantWindow {
    ino: u64,
    block_idx: u32,
    mapping: String,
    off: u64,
    end: u64,
}

impl TenantWindow {
    fn intersects_at_different_off(&self, other: &Self) -> bool {
        self.off != other.off && self.off < other.end && other.off < self.end
    }
}

/// What C12's decoder says about one mapping's window.
enum WindowClass {
    /// A window inside the chunk starting on the LBA grain.
    Window { off: u64, end: u64 },
    /// C12Overrun — the decoration is present but breaks the law or does
    /// not decode.
    Overrun(String),
}

/// C12's OWN tolerant decoder of the size-carrying form `base:off:len`
/// (base = `[proto://]offset[@inc]`) — deliberately NOT
/// [`crate::routing::DataRouter::parse_block_mapping`]: that one refuses
/// every violation with `EIO`, which is the READ path's contract; fsck
/// must REPORT what a read refuses, naming which law broke. The arithmetic
/// is the read law's verbatim (`LBA_GRAIN` alignment, `pack_slot_len`, the
/// allocator chunk), and it never panics on a hostile value (a `len` past
/// the chunk is refused before the grain round-up could overflow).
fn tenant_window_class(mapping: &str, chunk: u64) -> WindowClass {
    use crate::routing::{pack_slot_len, LBA_GRAIN};
    let rest = match mapping.find("://") {
        Some(pos) => &mapping[pos + 3..],
        None => mapping,
    };
    let mut parts = rest.split(':');
    let _base = parts.next();
    let Some(off_text) = parts.next() else {
        // Bare whole-block key.
        return WindowClass::Window { off: 0, end: chunk };
    };
    let (Some(len_text), None) = (parts.next(), parts.next()) else {
        return WindowClass::Overrun(format!(
            "decoration has {} component(s) after the base, not `off:len` — undecodable",
            rest.split(':').count() - 1
        ));
    };
    let Ok(off) = off_text.parse::<u64>() else {
        return WindowClass::Overrun(format!("undecodable rel_off '{off_text}'"));
    };
    let Ok(len) = len_text.parse::<u64>() else {
        return WindowClass::Overrun(format!("undecodable packed_len '{len_text}'"));
    };
    if off % LBA_GRAIN != 0 {
        return WindowClass::Overrun(format!(
            "rel_off {off} is not LBA_GRAIN ({LBA_GRAIN})-aligned"
        ));
    }
    if len > chunk {
        return WindowClass::Overrun(format!(
            "packed_len {len} reaches past the {chunk}-byte chunk on its own"
        ));
    }
    let end = off.saturating_add(pack_slot_len(len));
    if end > chunk {
        return WindowClass::Overrun(format!(
            "rel_off {off} + ceil(packed_len {len}) = {end} reaches past the {chunk}-byte chunk"
        ));
    }
    WindowClass::Window { off, end }
}

struct Suspect {
    kind: SuspectKind,
}

fn is_not_found(e: &SqueezefsError) -> bool {
    matches!(e, SqueezefsError::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// Run the detection engine. Report-only: never mutates. Findings must
/// be zero on a healthy volume (the tripwire).
pub async fn run(ctx: &FsckCtx, opts: &FsckOptions) -> Result<FsckReport> {
    let started = std::time::Instant::now();
    let mut counters = FsckCounters::default();
    let mut findings: Vec<FsckFinding> = Vec::new();
    let vols = volume_allocators(ctx);
    let online = opts.mode == FsckMode::Online;

    // Arm the scan latch + first epoch (online: the C2/C3 consistent
    // cut; offline probes have nothing in flight by definition). The
    // latch is disarmed on every exit path by the RAII release below.
    struct LatchRelease(Vec<Arc<BlockAllocator>>);
    impl Drop for LatchRelease {
        fn drop(&mut self) {
            for a in &self.0 {
                a.fsck_end_scan();
            }
        }
    }
    let _latch_release = if online && !opts.assume_latched {
        for v in &vols {
            v.alloc.fsck_begin_scan();
        }
        Some(LatchRelease(vols.iter().map(|v| v.alloc.clone()).collect()))
    } else {
        // Offline (nothing in flight by definition), or the fleet
        // fan-out holds the latch for the whole fleet window
        // (KD-MW-16): arming/releasing here would drain the epoch side
        // map mid-fleet.
        None
    };

    let mut suspects: Vec<Suspect> = Vec::new();
    let mut shard_refs: Option<PartialCensus> = None;
    // KD-PV-16: the volumes whose inode plane this run judged. Empty
    // until the plane actually records a verdict — a pass that covers
    // nothing must never read as covering something.
    let mut inode_plane_covered: Vec<usize> = Vec::new();

    if !opts.scrub_only {
        // ---- Pass 1: C1 walk + census + staging + accounting ----
        //
        // The C1 tree walks, the census, and the C4/C5 staging scan are
        // INDEPENDENT read-only scans; running them as a serial sum is
        // what put the measured scan rate under the G-VL-5(c)
        // ½-of-census floor (VL10). At 100 % throttle the per-(volume,
        // tree) C1 walks and the staging scan run as spawned tasks
        // concurrent with the census, so the pass wall clock is bounded
        // by the slowest single walk. Throttled runs keep the serial
        // shape: KD-3's duty cycle is per WORKER — concurrent walks
        // would consume a multiple of the granted duty budget.
        // `referenced` is C9's referenced-ino set — `None` when the dentry
        // pass could not complete, which SKIPS the class (a partial set is
        // never guessed from).
        let unthrottled = opts.throttle_pct == 0 || opts.throttle_pct >= 100;
        let (census, referenced) = if opts.inode_plane_only {
            // KD-PV-16's OWNER SHARD: the inode plane and nothing else.
            // The census residue partition (and with it C1/C2/C3/C6/C8,
            // the staging scan and the mapping list) belongs to the
            // coordinator's own shard set — a plane shard that reported
            // one would double-count it at the merge — so this walk skips
            // the layout extraction entirely and costs one inode-key scan
            // plus the whole-set dentry pass §5.8.0 states.
            let refs_pass = crate::meta_exec::spawn_meta_join("fsck_c9_refs", {
                let meta = ctx.meta.clone();
                let pct = opts.throttle_pct;
                let cancel = opts.cancel.clone();
                async move { build_referenced_inos(meta, None, pct, cancel, None).await }
            });
            let census = walk_census(ctx, opts, &mut counters).await?;
            let (refs, indexed, foreign_dentry) = refs_pass.await.map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "fsck C9 referenced-ino pass failed: {e}"
                ))
            })?;
            counters.dentry_refs_indexed += indexed;
            counters.inode_plane_foreign_dentry_scoped += foreign_dentry;
            counters.inodes_scanned = census.inodes_scanned;
            (census, refs)
        } else if unthrottled {
            // The C1 fan-out is BOUNDED by the meta lane population (the
            // census's own throttle law — review round 1 of PR 12b, Issue
            // 13): on a forest a unit is one SLOT TREE, and an N-writer set
            // holds hundreds (64 rotor + first-touched slots per writer per
            // volume), so one task per unit put hundreds of concurrent
            // walks on a two-lane pool — the venue where the loader's
            // mixed-style bucket lock wedged the fleet's reader. The walks
            // run in waves of `lanes × 4`, each wave awaited before the
            // next is spawned (results only accumulate — order is free).
            let wave = crate::meta_exec::meta_lanes_from(crate::cpu::process_parallelism())
                .saturating_mul(4)
                .max(1);
            let mut units: Vec<(
                usize,
                Arc<crate::meta_backend::kv::backend::KvMetaBackend>,
                C1Unit,
            )> = Vec::new();
            for (vol_idx, kv) in ctx.meta.volumes.iter().enumerate() {
                let (vol_units, scoped) = c1_units(kv).await;
                counters.c1_projection_slots_scoped += scoped;
                for unit in vol_units {
                    units.push((vol_idx, kv.clone(), unit));
                }
            }
            let mut walks = Vec::with_capacity(units.len());
            for chunk in units.chunks(wave) {
                let mut inflight = Vec::with_capacity(chunk.len());
                for (vol_idx, kv, unit) in chunk.iter().cloned() {
                    let cancel = opts.cancel.clone();
                    let pct = opts.throttle_pct;
                    inflight.push(crate::meta_exec::spawn_meta_join(
                        "fsck_c1_walk",
                        async move { walk_one_tree_c1(kv, vol_idx, unit, pct, cancel).await },
                    ));
                }
                for w in inflight {
                    walks.push(w.await);
                }
            }
            // The C4/C5 staging scan overlaps too (its own dirs +
            // per-custody-key getattr — disjoint from the walks).
            let staging = crate::meta_exec::spawn_meta_join(
                "fsck_staging_scan",
                scan_staging(
                    ctx.meta.clone(),
                    ctx.staging_dirs.clone(),
                    ctx.expected_generation.clone(),
                    // KD-MW-16: fleet shards scan THIS member's staging
                    // FULL — dirs are per-mount, so locality (not the
                    // ino residue) is the exactly-once partition here.
                    if opts.staging_full { None } else { opts.shard },
                ),
            );
            // C9's referenced-ino pass: one dentry-tree walk, disjoint
            // from the census (which walks `TREE_INODES`), so it overlaps
            // too — the difference is taken after both land, and neither
            // side depends on the other's order.
            let refs_pass = crate::meta_exec::spawn_meta_join("fsck_c9_refs", {
                let meta = ctx.meta.clone();
                let shard = opts.shard;
                let pct = opts.throttle_pct;
                let cancel = opts.cancel.clone();
                async move { build_referenced_inos(meta, shard, pct, cancel, None).await }
            });
            let census = walk_census(ctx, opts, &mut counters).await?;
            // All walks must land before the pass proceeds; awaiting in
            // submission order is equivalent (results only accumulate).
            for walk in walks {
                let (nodes_walked, walk_suspects) = walk.map_err(|e| {
                    crate::error::SqueezefsError::InvalidOperation(format!(
                        "fsck C1 walk task failed: {e}"
                    ))
                })?;
                counters.nodes_walked += nodes_walked;
                suspects.extend(walk_suspects);
            }
            suspects.extend(staging.await.map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "fsck staging scan task failed: {e}"
                ))
            })?);
            let (refs, indexed, foreign_dentry) = refs_pass.await.map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "fsck C9 referenced-ino pass failed: {e}"
                ))
            })?;
            counters.dentry_refs_indexed += indexed;
            counters.inode_plane_foreign_dentry_scoped += foreign_dentry;
            counters.inodes_scanned = census.inodes_scanned;
            if opts.shard.is_some() {
                shard_refs = Some(PartialCensus {
                    refs: census.refs.clone(),
                    mappings: census.mappings.clone(),
                    mappings_complete: true,
                });
            }
            (census, refs)
        } else {
            let mut c1 = Vec::new();
            walk_trees_c1(ctx, opts, &mut counters, &mut c1).await;
            suspects.extend(c1);
            let census = walk_census(ctx, opts, &mut counters).await?;
            counters.inodes_scanned = census.inodes_scanned;
            if opts.shard.is_some() {
                shard_refs = Some(PartialCensus {
                    refs: census.refs.clone(),
                    mappings: census.mappings.clone(),
                    mappings_complete: true,
                });
            }
            // C4/C5 staging scan (serial under throttle — KD-3's duty
            // cycle is per worker).
            let staging = scan_staging(
                ctx.meta.clone(),
                ctx.staging_dirs.clone(),
                ctx.expected_generation.clone(),
                if opts.staging_full { None } else { opts.shard },
            )
            .await;
            suspects.extend(staging);
            let (refs, indexed, foreign_dentry) = build_referenced_inos(
                ctx.meta.clone(),
                opts.shard,
                opts.throttle_pct,
                opts.cancel.clone(),
                None,
            )
            .await;
            counters.dentry_refs_indexed += indexed;
            counters.inode_plane_foreign_dentry_scoped += foreign_dentry;
            (census, refs)
        };

        // C2/C3/C6 evaluation against the live allocator state.
        if !opts.inode_plane_only {
            evaluate_allocator_classes(&vols, &census, opts, &mut counters, &mut suspects);
        }
        // C8 (pre-RC spec §6.2 item 1): durable-vs-derived block-reference
        // drift. The durable ledger holds one record per layout map entry and
        // the oracle counts one reference per layout map entry — through the
        // same key-resolution code — so a disagreement is real divergence,
        // not a race. Skipped on volumes without incompat bit 8 (no ledger
        // to disagree with) and under `--shards` (a shard sees only part of
        // the reference set, so its census is not comparable).
        //
        // **Detection is UNGATED** (the env gate the item-1 landing carried
        // is gone): the write-path wiring is complete, so
        // `meta_kv_block_refs_drift` is a live must-stay-0 tripwire. It
        // reached that state by using this very class as the checklist — the
        // last gap was the *deferred-accounting* class (a site that mutates
        // the RAM map and leaves the layout dirty, so the save that
        // eventually persists it is handed a map already containing the
        // change and stages nothing), closed structurally by the per-ino
        // deferred-op accumulator rather than site by site.
        //
        // **Cost when nothing drifts.** The comparison runs the layout walk
        // — but this is fsck, which walks every inode anyway for C1/C2/C3,
        // and the census reuses that same extraction. The added work is one
        // paged range scan of the reference tree per data volume (≈ 2.9 ns
        // per reference to decode) plus a `BTreeMap` fold and diff
        // (≈ 90 ns/reference) — `.benchmarks/2026-08-04-durable-block-refcounts.md`
        // §3–4. Detection therefore adds no walk that fsck did not already
        // owe, which is exactly why it can be unconditional HERE while
        // MOUNT keeps it behind `SQUEEZEFS_BLOCK_REFS_VERIFY=1` (there the
        // walk is the whole cost the durable records exist to delete).
        if opts.shard.is_none() && !opts.inode_plane_only {
            evaluate_c8(ctx, &mut suspects).await;
            // C11 (kvmap map plane, design-kvmap-block-map-tree §3 fsck):
            // the C8 posture exactly — unsharded only (a shard's census
            // sees a residue subset, and the orphan skip-scan is one
            // whole-tree question), report-only, verify-before-report.
            evaluate_c11_orphans(ctx, &mut counters, &mut suspects).await;
            // C11 (c) — PR 6a (design §12): run-vs-point coverage sanity,
            // the same unsharded posture.
            evaluate_c11_run_coverage(ctx, &mut counters, &mut suspects).await;
            evaluate_c11_empty_heads(&census, &mut counters, &mut suspects, &ctx.meta);
            // C12 (design-small-file-packing §5.9): tenant-range
            // consistency over the SAME census mapping list — a shard's
            // residue would see one tenant of a pair and judge nothing, so
            // the class rides the unsharded run (and the fleet finalize's
            // merged list), the C8 posture.
            evaluate_c12_tenant_ranges(&census, &mut suspects);
            // C13 (design-symmetric-metadata §5.8.5): orphan image extents
            // of the appender grants — a per-volume question over the
            // whole tree population, so unsharded like C8.
            evaluate_c13(ctx, &mut suspects).await;
            // C16 (design §5.8.5, PR 7): shared-index drift — the
            // flag side and the index side of one data volume's shared
            // blocks; unsharded like C8 (the ONE-walk census PR 1 owed is
            // still per kind, so the class rides C8's pass beside it).
            evaluate_c16(ctx, &mut suspects).await;
            // C17 (design §5.6.5, PR 7b): stripe consistency over the
            // marker census the ONE dentry walk carried — the striped
            // population alone is re-read; unsharded like C8.
            if let Some(refs) = referenced.as_ref() {
                evaluate_c17(ctx, &refs.markers, &mut suspects).await;
            }
            // C14 / C15 (design §5.8.5, PR 10): the slot custody census —
            // the appender directory's Live attestations against tree 0
            // and the death ledger; per meta volume, unsharded like C13
            // (one directory read + one tree-0 scan per volume).
            evaluate_c14_c15(ctx, &mut suspects).await;
        }

        // C9 (the class design-cow-kv-metadata §4.10a owed): live
        // inodes that no dentry names. Two independent bitmaps, one
        // difference — the era floor is applied per candidate inside, so
        // a healthy volume pays only the bitmap scan.
        //
        // C10 rides the SAME two sets in the other direction (plus the
        // name counts the same pass carried): nlink vs the names, and
        // everything the referenced set names that the live set does not.
        //
        // The inode plane is a ONE-VIEW plane (see `FsckOptions::
        // inode_plane`): a fleet MEMBER's shard skips it here — its
        // referenced set still feeds the census merge — while an OWNER
        // shard evaluates it over the volumes it appends to (KD-PV-16),
        // and `run_fleet` merges the two through the §5.8.2 predicate.
        //
        // §5.9.2's freeze precondition is checked FIRST and it is
        // self-certifying: a monotone checkpoint-consistent projection
        // that shows a peer volume's own `owner` field shows every commit
        // that preceded it there, including every cross-owner dentry that
        // will ever exist. Unassigned ⇒ the cross-owner reference set is
        // not frozen ⇒ no verdict (the existing incomplete-pass law), not
        // a verdict taken over a set that may still be growing names.
        let frozen = !opts.multi_owner || peer_volumes_are_assigned(ctx, opts).await;
        // The inode plane's TWO completeness laws on a forest, composed
        // (the verdict gate the PR 7b / PR 10 rebase names):
        //
        // 1. A NON-WRITER (a probe, a `-o ro` reader) with any `Live`
        //    appender page records NO verdict — `build_referenced_inos`
        //    answered `None` above (symmetric PR 7b, review round 1,
        //    Issue 21b). A blanket, and the right one there: the fixed
        //    ring's records a probe's replay skipped are its OWN region's
        //    window, which the per-ino scoping below treats as replayed.
        //
        // 2. The WRITER's online plane (symmetric PR 10, design §5.8.5 /
        //    §5.9): a FOREIGN appender's ring window this open did not
        //    replay (a live peer's acked records ahead of its checkpoint,
        //    or a dead appender's window before its recovery) holds
        //    dentries no tree names yet — a cross-owner create's name
        //    lives in the parent's holder's ring while the child's record
        //    is the creator's; a cross-owner unlink's DELETE removes a
        //    name whose child's `nlink` moved in the child's slot. The
        //    referenced set is INCOMPLETE over the INOS such a window
        //    names, so the plane SCOPES those inos out
        //    (`fsck_inode_plane_window_scoped`) and judges every other —
        //    never skipping a live fleet whole (review round 1, Issue 12):
        //    each foreign ring is read ONCE per census, its window's inode
        //    keys and dentry targets (a DELETE's resolved in the tree —
        //    review round 2, Issue 26) are the exclusion. `None` = a ring
        //    could not be read: unknown = pending, the whole plane takes no
        //    verdict this run.
        let window_inos = foreign_window_inos(ctx).await;
        if let Some(w) = window_inos.as_ref().filter(|w| !w.is_empty()) {
            log::warn!(
                "fsck inode plane: {} ino(s) named by foreign appender ring windows this open \
                 did not replay — scoped out of C9/C10 this run (their holder's checkpoint or \
                 recovery completes the referenced set)",
                w.len()
            );
        }
        match (
            referenced
                .as_ref()
                .filter(|_| opts.inode_plane)
                .filter(|_| frozen && window_inos.is_some()),
            census.live.truncated(),
        ) {
            (Some(refs), false) => {
                let window_inos = window_inos.clone().unwrap_or_default();
                evaluate_c9_unreferenced(
                    ctx,
                    opts,
                    &census.live,
                    &refs.refs,
                    &window_inos,
                    &mut counters,
                    &mut suspects,
                )
                .await;
                evaluate_c10_inode_plane(
                    ctx,
                    opts,
                    &census,
                    refs,
                    &window_inos,
                    &mut counters,
                    &mut suspects,
                )
                .await;
                inode_plane_covered = opts.covered_volumes(ctx.meta.volumes.len());
            }
            (Some(_), true) => log::warn!(
                "fsck C9/C10: the live-inode set reached its derived byte budget ({} B) — \
                 the inode-plane classes record no verdict for this run",
                ino_set_byte_budget()
            ),
            (None, _) => {}
        }

        counters.suspects = suspects.len() as u64;

        if !suspects.is_empty() {
            if online {
                // ---- Settle (§5.6 step 2) ----
                squeezefs_ipc::sqz_time::sleep(opts.settle).await;
                for v in &vols {
                    v.alloc.fsck_bump_epoch();
                }
            }
            recheck_suspects(ctx, opts, &vols, suspects, &mut counters, &mut findings).await?;
        }
    }

    // ---- C7 scrub (KD-17) ----
    if (opts.scrub || opts.scrub_only) && !opts.inode_plane_only {
        // Fresh mapping set (post-settle when checks ran): scrub what is
        // referenced NOW.
        let census = walk_census(ctx, opts, &mut counters).await?;
        if counters.inodes_scanned == 0 {
            counters.inodes_scanned = census.inodes_scanned;
        }
        if opts.shard.is_some() && shard_refs.is_none() {
            shard_refs = Some(PartialCensus {
                refs: census.refs.clone(),
                mappings: census.mappings.clone(),
                mappings_complete: true,
            });
        }
        scrub_c7(ctx, opts, &census, &mut counters, &mut findings).await;
    }

    counters.findings = findings.len() as u64;
    counters.inode_plane_volumes_covered = inode_plane_covered.len() as u64;
    // PR 8: the lessee-shard coverage — `leased ∪ unleased` per judged
    // volume, summed (every hosted slot on an unarmed volume).
    for &vi in &inode_plane_covered {
        if let Some(kv) = ctx.meta.volumes.get(vi) {
            if let Ok(cov) = kv.inode_plane_slot_coverage().await {
                counters.inode_plane_slots_covered += cov.covered;
            }
        }
    }
    // KD-PV-16: a scoped pass says so. On a mount that owns part of a set
    // the local run covers its own volumes and NOTHING else — the peers'
    // planes are their owners' shards (`run_fleet`), so a bare `run` here
    // is an INCOMPLETE pass over the set, never a narrower verdict.
    if opts.multi_owner && counters.inode_plane_volumes_covered < ctx.meta.volumes.len() as u64 {
        log::warn!(
            "fsck: the inode plane covered {} of {} volumes ({:?}) — this pass is INCOMPLETE \
             for the SET: the remaining volumes are judged by their own owners' shards \
             (design-per-volume-claim-admission §5.8.1)",
            counters.inode_plane_volumes_covered,
            ctx.meta.volumes.len(),
            inode_plane_covered
        );
    }
    counters.scan_secs = started.elapsed().as_secs();
    findings.sort();
    findings.dedup();
    publish_metrics(&counters);

    Ok(FsckReport {
        schema: FSCK_REPORT_SCHEMA,
        mode: match opts.mode {
            FsckMode::Online => "online".to_string(),
            FsckMode::Offline => "offline".to_string(),
        },
        shard: opts.shard.map(|(k, n)| format!("{k}/{n}")),
        findings,
        counters: counters.clone(),
        partial: shard_refs,
        repair: None,
        inode_plane_covered,
        // A durable/offline report never elides — only the admin-lane
        // VIEW (`to_bounded_json`) mints a nonzero count.
        findings_elided: 0,
    })
}

/// §5.9.2's **freeze precondition**, read as ONE durable observable fact
/// per peer-owned volume: does its (monotone, checkpoint-consistent)
/// projection carry a durable `claim_set.owner`?
///
/// While a multi-owner plane is armed no cross-owner dentry can be
/// created (M2 constrains `create`, M1 refuses cross-owner
/// `link`/`rename`/`unlink`/`rmdir`, row 13 refuses cross-owner slot
/// migration), so the cross-owner reference set is FIXED at the
/// assignment instant. A projection showing the assignment therefore
/// shows every cross-owner name that will ever exist on that volume —
/// which is what makes an owner's plane pass read only records it
/// appends to plus records that CANNOT change. No barrier verb, no ack
/// ledger, no timeout: one fact, monotone thereafter.
///
/// `false` ⇒ the plane records NO verdict for this run (the existing
/// incomplete-pass law).
async fn peer_volumes_are_assigned(ctx: &FsckCtx, opts: &FsckOptions) -> bool {
    for (v_idx, kv) in ctx.meta.volumes.iter().enumerate() {
        if opts.owns_volume(v_idx) {
            continue;
        }
        let assigned = crate::membership::ClaimSet::load(kv)
            .await
            .is_some_and(|s| s.durable && s.owner.is_some());
        if !assigned {
            log::warn!(
                "fsck C9/C10: peer-owned volume {v_idx} ({}) shows no durable ownership \
                 assignment in this mount's projection — the cross-owner reference set is \
                 not yet frozen (design-per-volume-claim-admission §5.9.2), so the \
                 inode-plane classes record NO verdict for this run",
                kv.device_path().display()
            );
            return false;
        }
    }
    true
}

/// Union shard reports (`--shards k/N` outputs): findings dedupe by
/// identity (per-shard-repeated classes C1/C5 collapse), census
/// counters sum, per-run gauges take the max.
pub fn merge_reports(reports: &[FsckReport]) -> FsckReport {
    let mut findings: Vec<FsckFinding> = Vec::new();
    let mut counters = FsckCounters::default();
    let mut refs: HashMap<String, HashMap<u64, u32>> = HashMap::new();
    let mut mappings: Vec<MappingRef> = Vec::new();
    let mut mappings_complete = true;
    let mut mode = "offline".to_string();
    let mut inode_plane_covered: Vec<usize> = Vec::new();
    for r in reports {
        findings.extend(r.findings.iter().cloned());
        mode.clone_from(&r.mode);
        counters.inodes_scanned += r.counters.inodes_scanned;
        // C9: every shard walks the WHOLE dentry tree but indexes only
        // its own residue, so both of these sum (disjoint by residue).
        counters.dentry_refs_indexed += r.counters.dentry_refs_indexed;
        counters.current_era_exempted += r.counters.current_era_exempted;
        counters.unreferenced_intent_exempted += r.counters.unreferenced_intent_exempted;
        // C10: every shard judges its own ino residue, so these sum too.
        counters.nlink_names_counted += r.counters.nlink_names_counted;
        counters.nlink_mismatch_high += r.counters.nlink_mismatch_high;
        counters.nlink_mismatch_low += r.counters.nlink_mismatch_low;
        counters.nlink_zero_named += r.counters.nlink_zero_named;
        counters.dangling_dentries += r.counters.dangling_dentries;
        counters.nlink_transient_cleared += r.counters.nlink_transient_cleared;
        counters.inode_plane_foreign_scoped += r.counters.inode_plane_foreign_scoped;
        counters.inode_plane_foreign_slot_scoped += r.counters.inode_plane_foreign_slot_scoped;
        counters.inode_plane_window_scoped += r.counters.inode_plane_window_scoped;
        counters.inode_plane_foreign_dentry_scoped += r.counters.inode_plane_foreign_dentry_scoped;
        counters.c1_projection_slots_scoped += r.counters.c1_projection_slots_scoped;
        counters.inode_plane_slots_covered += r.counters.inode_plane_slots_covered;
        counters.inode_plane_cross_owner_declined += r.counters.inode_plane_cross_owner_declined;
        counters.inode_plane_proposals_admitted += r.counters.inode_plane_proposals_admitted;
        counters.inode_plane_proposals_stripped += r.counters.inode_plane_proposals_stripped;
        // KD-PV-16: coverage is a UNION of volume identities, never a
        // sum — two shards that judged the same volume covered ONE
        // volume, and a sum would let a duplicated shard read as full
        // coverage of a set it never reached.
        for v in &r.inode_plane_covered {
            if !inode_plane_covered.contains(v) {
                inode_plane_covered.push(*v);
            }
        }
        counters.blocks_checked += r.counters.blocks_checked;
        counters.refcounts_checked += r.counters.refcounts_checked;
        counters.suspects += r.counters.suspects;
        counters.suspects_cleared += r.counters.suspects_cleared;
        counters.epoch_exempted += r.counters.epoch_exempted;
        counters.inflight_exempted += r.counters.inflight_exempted;
        counters.mover_ledger_exempted += r.counters.mover_ledger_exempted;
        counters.pack_ledger_exempted += r.counters.pack_ledger_exempted;
        counters.foreign_lane_exempted += r.counters.foreign_lane_exempted;
        counters.alloc_bitmap_leak_candidates += r.counters.alloc_bitmap_leak_candidates;
        counters.alloc_bitmap_tracked_exempted += r.counters.alloc_bitmap_tracked_exempted;
        counters.map_orphan_records += r.counters.map_orphan_records;
        counters.map_empty_heads += r.counters.map_empty_heads;
        counters.map_run_foreign_shadows += r.counters.map_run_foreign_shadows;
        counters.crossing_exempted += r.counters.crossing_exempted;
        counters.tenant_overlap_findings += r.counters.tenant_overlap_findings;
        counters.shared_index_drift += r.counters.shared_index_drift;
        counters.stripe_findings += r.counters.stripe_findings;
        counters.slot_custody_conflicts += r.counters.slot_custody_conflicts;
        counters.unrecovered_appenders += r.counters.unrecovered_appenders;
        counters.scrub_blocks_scanned += r.counters.scrub_blocks_scanned;
        counters.scrub_bytes_scanned += r.counters.scrub_bytes_scanned;
        counters.scrub_aead_verified += r.counters.scrub_aead_verified;
        counters.scrub_frame_verified += r.counters.scrub_frame_verified;
        counters.scrub_readability_only += r.counters.scrub_readability_only;
        counters.scrub_failures += r.counters.scrub_failures;
        counters.nodes_walked = counters.nodes_walked.max(r.counters.nodes_walked);
        counters.scan_secs = counters.scan_secs.max(r.counters.scan_secs);
        if let Some(p) = &r.partial {
            for (vol, m) in &p.refs {
                let e = refs.entry(vol.clone()).or_default();
                for (off, c) in m {
                    *e.entry(*off).or_insert(0) += c;
                }
            }
            // KD-MW-16: mapping identities concatenate (residues are
            // disjoint by construction); one degraded shard degrades
            // the union — the finalize's walk fallback is loud.
            mappings.extend(p.mappings.iter().cloned());
            mappings_complete &= p.mappings_complete;
        } else {
            // A report with NO partial census (an unsharded input)
            // carries no mapping identities to merge.
            mappings_complete = false;
        }
    }
    findings.sort();
    findings.dedup();
    counters.findings = findings.len() as u64;
    inode_plane_covered.sort_unstable();
    counters.inode_plane_volumes_covered = inode_plane_covered.len() as u64;
    FsckReport {
        schema: FSCK_REPORT_SCHEMA,
        mode,
        shard: None,
        findings,
        counters,
        partial: Some(PartialCensus {
            refs,
            mappings,
            mappings_complete,
        }),
        repair: None,
        inode_plane_covered,
        // Shard/offline reports never elide (the admin VIEW's field);
        // summing would double-speak if a bounded view were ever fed
        // back in, and merge inputs are durable reports by contract.
        findings_elided: 0,
    }
}

// ---------------------------------------------------------------------------
// KD-MW-16 (rung 10c) — the fleet fan-out/merge/finalize engine
// ---------------------------------------------------------------------------

/// Fold the FINALIZE pass's counters into a merged report's (the fields
/// the allocator classes + verify ladder move; census fields stay the
/// shards').
fn fold_finalize_counters(dst: &mut FsckCounters, fin: &FsckCounters) {
    dst.nodes_walked = dst.nodes_walked.max(fin.nodes_walked);
    dst.blocks_checked += fin.blocks_checked;
    dst.refcounts_checked += fin.refcounts_checked;
    dst.suspects += fin.suspects;
    dst.suspects_cleared += fin.suspects_cleared;
    dst.epoch_exempted += fin.epoch_exempted;
    dst.inflight_exempted += fin.inflight_exempted;
    dst.mover_ledger_exempted += fin.mover_ledger_exempted;
    dst.pack_ledger_exempted += fin.pack_ledger_exempted;
    dst.foreign_lane_exempted += fin.foreign_lane_exempted;
    dst.alloc_bitmap_leak_candidates += fin.alloc_bitmap_leak_candidates;
    dst.alloc_bitmap_tracked_exempted += fin.alloc_bitmap_tracked_exempted;
    // C11 runs ONLY in the finalize (shards skip the map plane, so the
    // shard reports carry zeros — no double count).
    dst.map_orphan_records += fin.map_orphan_records;
    dst.map_empty_heads += fin.map_empty_heads;
    dst.map_run_foreign_shadows += fin.map_run_foreign_shadows;
    dst.crossing_exempted += fin.crossing_exempted;
    // C12 rides the finalize like C8 (the merged mapping list is the one
    // whole census; shards judge no tenant ranges).
    dst.tenant_overlap_findings += fin.tenant_overlap_findings;
    dst.shared_index_drift += fin.shared_index_drift;
    dst.stripe_findings += fin.stripe_findings;
    dst.slot_custody_conflicts += fin.slot_custody_conflicts;
    dst.unrecovered_appenders += fin.unrecovered_appenders;
    // **The inode plane's counters are the union of the ADMITTED shards
    // and this finalize** (KD-PV-16, §5.8.2 F4 — the premise that the
    // plane "exists only here" is what that decision retires). The two
    // halves are disjoint by construction and each is folded exactly
    // once: an admitted owner shard's counters ride its REPORT through
    // `merge_reports` (a stripped shard's are zeroed there), and the
    // coordinator's own plane — over the volumes IT appends to — rides
    // this fold. Adding an admitted shard's counters here as well would
    // double-count every owner's findings.
    dst.dentry_refs_indexed += fin.dentry_refs_indexed;
    dst.current_era_exempted += fin.current_era_exempted;
    dst.unreferenced_intent_exempted += fin.unreferenced_intent_exempted;
    dst.nlink_names_counted += fin.nlink_names_counted;
    dst.nlink_mismatch_high += fin.nlink_mismatch_high;
    dst.nlink_mismatch_low += fin.nlink_mismatch_low;
    dst.nlink_zero_named += fin.nlink_zero_named;
    dst.dangling_dentries += fin.dangling_dentries;
    dst.nlink_transient_cleared += fin.nlink_transient_cleared;
    dst.inode_plane_foreign_scoped += fin.inode_plane_foreign_scoped;
    dst.inode_plane_foreign_slot_scoped += fin.inode_plane_foreign_slot_scoped;
    dst.inode_plane_window_scoped += fin.inode_plane_window_scoped;
    dst.inode_plane_foreign_dentry_scoped += fin.inode_plane_foreign_dentry_scoped;
    dst.c1_projection_slots_scoped += fin.c1_projection_slots_scoped;
    dst.inode_plane_slots_covered += fin.inode_plane_slots_covered;
    dst.inode_plane_cross_owner_declined += fin.inode_plane_cross_owner_declined;
    // `inode_plane_volumes_covered` is deliberately NOT folded: coverage
    // is a UNION of volume identities the caller composes (§5.8.1), and
    // adding two shards' counts would let a set covered twice read as a
    // set covered whole.
}

/// The FLOOR of the fleet collect loop's progress deadline: the job wire's
/// own dial/handshake deadline — a worker's proposal is one wire round
/// trip, and "late" cannot be judged below the bound the wire itself
/// grants a single dial (PR 12b review round 2, Issue 27; tie-tested in
/// `derivation_sweep_tests`). Below the lease TTL in every shipped
/// configuration; the floor binds only where a harness shortens the TTL.
pub fn fleet_collect_progress_floor() -> Duration {
    crate::job_wire::ENROLL_DIAL_TIMEOUT
}

/// The **fleet fsck detect pass** (KD-MW-16, `docs/design-mw-fleet-jobs.md`
/// §4): shard the census across enrolled fleet read workers, merge their
/// fencing-checked proposals through the EXISTING `merge_reports` union
/// law, then FINALIZE the allocator classes (C2 tracked / C3 / C6) + C8
/// on the coordinator over the fleet-merged full reference census, with
/// the EXISTING verify-before-report ladder.
///
/// **Zero capacity is the identity** (pinned): with no dispatch — or no
/// enrolled read-capable worker — this IS [`run`], byte-for-byte the
/// shipped single-writer path. A lost shard (TTL expiry, worker abandon,
/// refused/malformed proposal) is RE-LEASED: re-dispatched to another
/// idle capable worker, else run locally (`job_fleet_shards_relocal`) —
/// every residue lands exactly once, and a stale proposal never merges
/// (the fencing law, `job_remote_refused_stale`).
pub async fn run_fleet(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    fleet: Option<Arc<dyn crate::jobs::FleetDispatch>>,
    job_id: &str,
) -> Result<FsckReport> {
    let Some(fleet) = fleet else {
        return run(ctx, opts).await;
    };
    // The fan-out is the ONLINE coordinator's (the fabric job); offline
    // probes keep their zero-coordination `--shards` contract.
    if opts.mode != FsckMode::Online || fleet.read_capacity() == 0 {
        return run(ctx, opts).await;
    }
    let started = std::time::Instant::now();

    // Arm the allocator scan latch for the WHOLE fleet window: the
    // C2/C3 allocation-epoch side map must span every shard walk, and
    // the inner runs are told the latch is held (`assume_latched`) so
    // no nested release can drain it mid-fleet.
    let vols = volume_allocators(ctx);
    for v in &vols {
        v.alloc.fsck_begin_scan();
    }
    struct FleetLatchRelease(Vec<Arc<BlockAllocator>>);
    impl Drop for FleetLatchRelease {
        fn drop(&mut self) {
            for a in &self.0 {
                a.fsck_end_scan();
            }
        }
    }
    let latch_release = FleetLatchRelease(vols.iter().map(|v| v.alloc.clone()).collect());

    let job_type = crate::jobs::JobType::Fsck {
        scrub: opts.scrub,
        scrub_only: opts.scrub_only,
        repair: false,
        apply: false,
        quarantine_dir: None,
    };
    let (tx, mut rx) = squeezefs_ipc::sqz_channel::mpsc::unbounded_channel();

    // ---- KD-PV-16: one INODE-PLANE shard per peer OWNER ----
    //
    // The census fan-out above shards by ino RESIDUE across whatever
    // read-capable members exist; the inode plane cannot ride it, because
    // its candidate scope is OWNERSHIP (KD-PV-7) and a residue-scoped
    // pass over a subset of an owner's inos would cover 1/n of each of
    // its volumes. So the plane is dispatched separately and TARGETED:
    // the coordinator asks each peer that its own `OwnerMap` names, and
    // an owner it cannot reach leaves that owner's volumes UNCOVERED
    // (loud) rather than silently unevaluated. A lost plane shard is
    // deliberately NOT relocal-able: evaluating a peer's inos here is the
    // false-positive generator KD-PV-7 exists to refuse.
    let owner_map = crate::meta_ship::owners::owner_map().filter(|m| m.multi_owner());
    let mut plane_outstanding: std::collections::HashMap<u32, String> =
        std::collections::HashMap::new();
    let mut plane_retried: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut plane_unreachable: Vec<(String, Vec<usize>)> = Vec::new();
    if let Some(map) = owner_map.as_ref() {
        for (i, peer) in map.peers().iter().enumerate() {
            let shard_no = crate::jobs::INODE_PLANE_SHARD_BASE + i as u32;
            let owned = map.volumes_owned_by(&peer.peer_id);
            if fleet.dispatch_inode_plane_shard(
                job_id,
                shard_no,
                &peer.peer_id,
                &job_type,
                opts.throttle_pct,
                &tx,
            ) {
                plane_outstanding.insert(shard_no, peer.peer_id.clone());
            } else {
                plane_unreachable.push((peer.peer_id.clone(), owned));
            }
        }
        log::info!(
            "fsck fleet ({job_id}): inode plane fanned out to {} owner shard(s), {} owner(s) \
             unreachable (KD-PV-16); this coordinator judges volumes {:?}",
            plane_outstanding.len(),
            plane_unreachable.len(),
            opts.covered_volumes(ctx.meta.volumes.len())
        );
    }

    // The census partition is sized by what is STILL idle: a node has one
    // session, so an owner serving its plane shard is not also a census
    // venue this pass. Sizing `n` after the plane fan-out is what keeps
    // the residue partition exactly-once instead of handing residues to
    // sessions that are already busy (they would fall back to relocal).
    let workers = fleet.read_capacity() as u32;
    let n = workers.saturating_add(1);
    let mut outstanding: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut local_residues: Vec<u32> = Vec::new();
    for k in 1..n {
        if fleet.dispatch_read_shard(job_id, k, n, &job_type, opts.throttle_pct, &tx) {
            outstanding.insert(k);
        } else {
            local_residues.push(k);
        }
    }
    log::info!(
        "fsck fleet ({job_id}): census partitioned {n} ways — {} worker shard(s) \
         dispatched, {} residue(s) local (+ the coordinator's shard 0)",
        outstanding.len(),
        local_residues.len()
    );

    // The coordinator's own shard 0 — ONLINE, its suspects machinery
    // armed; the members run offline-posture over their coherent views.
    // The INODE PLANE (C9/C10) is skipped by every shard — local and
    // member alike — and judged WHOLE at finalize over the coordinator's
    // one coherent view (see `FsckOptions::inode_plane`): a member's
    // S5-bounded view mixes per-volume instants and manufactures the
    // count/name loss shapes from a healthy tree, and a locally-sharded
    // judgment beside a finalize-whole one would double-report residue 0.
    let local_shard = |k: u32| {
        let mut o = opts.clone();
        o.shard = Some((k, n));
        o.staging_full = true;
        o.assume_latched = true;
        o.inode_plane = false;
        o
    };
    let shard0_started = std::time::Instant::now();
    let mut reports = vec![run(ctx, &local_shard(0)).await?];
    // The collect loop's PROGRESS deadline (review round 1 of PR 12b,
    // Issue 13): a worker's shard is the same partition this coordinator
    // just ran as shard 0, so a proposal is due within a bounded multiple
    // of that wall — floored at the lease TTL (a shard that beats its
    // lease but never proposes is a WEDGED worker, which the lease law
    // alone never notices) and at the wire's own dial deadline (a shard 0
    // that finished in microseconds on an empty set must not read a
    // worker still inside its first round trip as wedged —
    // `fleet_collect_progress_floor`). Past it every outstanding census
    // shard is treated as lost: re-leased or run locally, and the job
    // TERMINATES.
    let shard0_wall = shard0_started.elapsed();
    let progress_deadline = std::time::Instant::now()
        + fleet.shard_lease_ttl().max(
            shard0_wall
                .saturating_mul(4)
                .max(fleet_collect_progress_floor()),
        );
    for k in std::mem::take(&mut local_residues) {
        METRICS
            .job_fleet_shards_relocal
            .fetch_add(1, Ordering::Relaxed);
        reports.push(run(ctx, &local_shard(k)).await?);
    }

    // Collect worker proposals; a lost/refused residue RE-LEASES —
    // another idle capable worker first, else locally.
    let mut worker_counters = FsckCounters::default();
    // KD-PV-16's ledger: what the owner shards contributed.
    let mut plane_admitted = 0u64;
    let mut plane_stripped = 0u64;
    let mut plane_covered: Vec<usize> = Vec::new();
    // Both shard populations land on ONE channel, so the collect runs
    // while EITHER is outstanding: with every capable session serving a
    // plane shard the census partition is empty, and a loop keyed on the
    // residues alone would return before the plane answered.
    while !outstanding.is_empty() || !plane_outstanding.is_empty() {
        if opts.cancel.load(Ordering::Relaxed) {
            // Fabric cancel/pause: stop collecting — the job layer owns
            // the terminal state, and a cancelled detect pass's partial
            // report is never adjudicated as complete.
            break;
        }
        let Ok(recv) =
            squeezefs_ipc::sqz_time::timeout(Duration::from_millis(250), rx.recv()).await
        else {
            if std::time::Instant::now() >= progress_deadline {
                let stuck: Vec<u32> = outstanding.iter().copied().collect();
                log::warn!(
                    "fsck fleet ({job_id}): census shard(s) {stuck:?} of {n} and {} inode-plane \
                     shard(s) proposed nothing within the progress deadline (lease TTL {:?} / \
                     4 x the coordinator's own shard wall {:?}) — a wedged or gone worker; the \
                     job's fleet shards are RETIRED (a late proposal is refused), the census \
                     shards run locally (job_fleet_shards_relocal), an owner's plane shard \
                     leaves the pass INCOMPLETE",
                    plane_outstanding.len(),
                    fleet.shard_lease_ttl(),
                    shard0_wall
                );
                fleet.retire_fleet_shards(job_id);
                for k in stuck {
                    outstanding.remove(&k);
                    METRICS
                        .job_fleet_shards_relocal
                        .fetch_add(1, Ordering::Relaxed);
                    reports.push(run(ctx, &local_shard(k)).await?);
                }
                break;
            }
            continue;
        };
        let Some(out) = recv else {
            break; // channel closed: the host is gone — relocal below
        };
        if let Some(peer_id) = plane_outstanding.get(&out.shard).cloned() {
            // ---- KD-PV-16: an OWNER's inode-plane proposal ----
            //
            // Clause 2 of the §5.8.2 predicate: the owned set comes from
            // the lease HOLDER's `worker_id` — read from the wire's own
            // lease table, never from the payload — looked up in THIS
            // node's `OwnerMap`. A worker the map does not name (a
            // reader, a co-writer, `{hostname}:{pid}`, an older binary)
            // resolves to the empty set, which is the pre-KD-PV-16 drop
            // path verbatim.
            let holder = out.worker_id.clone().unwrap_or_default();
            let owned = owner_map
                .as_ref()
                .map(|m| m.volumes_owned_by(&holder))
                .unwrap_or_default();
            let mut lost_reason: Option<String> = None;
            match out.payload {
                Some(bytes) => match serde_json::from_slice::<FsckReport>(&bytes) {
                    // A plane shard carries no ino residue: a report
                    // labelled with one answered a question nobody asked.
                    Ok(mut r) if r.shard.is_none() => {
                        plane_outstanding.remove(&out.shard);
                        let (admitted, stripped) =
                            admit_inode_plane_proposals(&mut r, &owned, &ctx.meta);
                        if stripped > 0 {
                            log::warn!(
                                "fsck fleet ({job_id}): inode-plane shard {} (holder '{holder}', \
                                 dispatched to '{peer_id}') proposed {stripped} finding(s) this \
                                 coordinator's own owner map does not entitle it to — dropped \
                                 (fsck_inode_plane_proposals_stripped; §5.8.2). Owned per this \
                                 map: {owned:?}",
                                out.shard
                            );
                        }
                        // The ledger is per PROPOSAL, not per finding: a
                        // healthy fleet has no findings at all, and
                        // "coverage that closes without an admitted
                        // proposal came from nowhere" is the reading
                        // §5.8.2 asks for.
                        plane_admitted += u64::from(!owned.is_empty());
                        plane_stripped += u64::from(stripped > 0);
                        log::debug!(
                            "fsck fleet ({job_id}): inode-plane shard {} admitted {admitted}                              finding(s) from owner '{holder}' over volumes {owned:?}",
                            out.shard
                        );
                        for v in &r.inode_plane_covered {
                            if !plane_covered.contains(v) {
                                plane_covered.push(*v);
                            }
                        }
                        // The plane shard walks inodes to build its own
                        // coherent view; that walk is NOT census coverage
                        // (the residue shards own that partition), so its
                        // census counters are dropped rather than summed
                        // — the exactly-once law of gate 1.
                        let mut c = FsckCounters {
                            dentry_refs_indexed: r.counters.dentry_refs_indexed,
                            current_era_exempted: r.counters.current_era_exempted,
                            unreferenced_intent_exempted: r.counters.unreferenced_intent_exempted,
                            nlink_names_counted: r.counters.nlink_names_counted,
                            nlink_mismatch_high: r.counters.nlink_mismatch_high,
                            nlink_mismatch_low: r.counters.nlink_mismatch_low,
                            nlink_zero_named: r.counters.nlink_zero_named,
                            dangling_dentries: r.counters.dangling_dentries,
                            nlink_transient_cleared: r.counters.nlink_transient_cleared,
                            inode_plane_foreign_scoped: r.counters.inode_plane_foreign_scoped,
                            inode_plane_cross_owner_declined: r
                                .counters
                                .inode_plane_cross_owner_declined,
                            ..Default::default()
                        };
                        std::mem::swap(&mut r.counters, &mut c);
                        r.partial = None;
                        r.inode_plane_covered.clear();
                        r.counters.findings = r.findings.len() as u64;
                        fold_worker_counters(&mut worker_counters, &r.counters);
                        reports.push(r);
                    }
                    Ok(r) => {
                        lost_reason = Some(format!(
                            "an inode-plane shard proposed a census residue label '{}'",
                            r.shard.as_deref().unwrap_or("<none>")
                        ));
                    }
                    Err(e) => lost_reason = Some(format!("undecodable shard report: {e}")),
                },
                None => lost_reason = Some("lease lost (expiry/abandon)".to_string()),
            }
            if let Some(why) = lost_reason {
                let retry = plane_retried.insert(out.shard)
                    && fleet.dispatch_inode_plane_shard(
                        job_id,
                        out.shard,
                        &peer_id,
                        &job_type,
                        opts.throttle_pct,
                        &tx,
                    );
                log::warn!(
                    "fsck fleet ({job_id}): inode-plane shard {} of owner '{peer_id}' was LOST \
                     ({why}) — {}",
                    out.shard,
                    if retry {
                        "re-leased to the same owner (only an owner may judge its own inos)"
                    } else {
                        "its volumes stay UNCOVERED: a peer's inode plane is never evaluated \
                         here (KD-PV-7), so the pass is INCOMPLETE rather than narrowed"
                    }
                );
                if !retry {
                    plane_outstanding.remove(&out.shard);
                    plane_unreachable.push((
                        peer_id.clone(),
                        owner_map
                            .as_ref()
                            .map(|m| m.volumes_owned_by(&peer_id))
                            .unwrap_or_default(),
                    ));
                }
            }
            continue;
        }
        if !outstanding.contains(&out.shard) {
            continue; // a duplicate notification for a settled residue
        }
        let mut lost_reason: Option<String> = None;
        match out.payload {
            Some(bytes) => match serde_json::from_slice::<FsckReport>(&bytes) {
                Ok(mut r)
                    if r.shard.as_deref() == Some(format!("{}/{}", out.shard, n).as_str()) =>
                {
                    outstanding.remove(&out.shard);
                    // The inode plane is inadmissible from a CENSUS
                    // residue shard by construction (one-view law — the
                    // module doc on `FsckOptions::inode_plane`): a fleet
                    // worker of this binary never proposes it here, so
                    // anything stripped is an older/foreign binary's
                    // time-shifted verdict — dropped LOUDLY, never
                    // merged, never counted on the coordinator's C9/C10
                    // tripwires. No coverage is lost: the plane is judged
                    // by the coordinator's finalize over its OWN volumes
                    // plus the KD-PV-16 owner shards above.
                    let (_, stripped) = admit_inode_plane_proposals(&mut r, &[], &ctx.meta);
                    if stripped > 0 {
                        plane_stripped += 1;
                        log::warn!(
                            "fsck fleet ({job_id}): census shard {}/{n} proposed {stripped} \
                             inode-plane (C9/C10) finding(s) — a residue shard's view is \
                             ino-residue-scoped and staleness-bounded, so it is inadmissible \
                             whatever the proposer owns (one-view law); dropped, and the \
                             coordinator + its owner shards judge the plane",
                            out.shard
                        );
                    }
                    // The coordinator's stats account for the WHOLE
                    // fleet pass (design §4 step 7); findings/scan_secs
                    // stay per-report (dedupe/max at merge).
                    let mut c = r.counters.clone();
                    c.findings = 0;
                    c.scan_secs = 0;
                    fold_worker_counters(&mut worker_counters, &c);
                    reports.push(r);
                }
                Ok(r) => {
                    lost_reason = Some(format!(
                        "shard identity mismatch: proposed '{}', expected '{}/{}'",
                        r.shard.as_deref().unwrap_or("<none>"),
                        out.shard,
                        n
                    ));
                }
                Err(e) => {
                    lost_reason = Some(format!("undecodable shard report: {e}"));
                }
            },
            None => {
                lost_reason = Some("lease lost (expiry/abandon)".to_string());
            }
        }
        if let Some(why) = lost_reason {
            log::warn!(
                "fsck fleet ({job_id}): shard {}/{n} was LOST ({why}) — re-leasing",
                out.shard
            );
            if !fleet.dispatch_read_shard(job_id, out.shard, n, &job_type, opts.throttle_pct, &tx) {
                outstanding.remove(&out.shard);
                METRICS
                    .job_fleet_shards_relocal
                    .fetch_add(1, Ordering::Relaxed);
                reports.push(run(ctx, &local_shard(out.shard)).await?);
            }
        }
    }
    // Host gone mid-pass (channel closed): every still-outstanding
    // residue runs locally — the pass never under-covers.
    for k in std::mem::take(&mut outstanding)
        .into_iter()
        .collect::<std::collections::BTreeSet<u32>>()
    {
        if opts.cancel.load(Ordering::Relaxed) {
            break;
        }
        METRICS
            .job_fleet_shards_relocal
            .fetch_add(1, Ordering::Relaxed);
        reports.push(run(ctx, &local_shard(k)).await?);
    }
    // The same event on the PLANE side has the opposite answer: an
    // owner's inos are judged by that owner or by nobody (KD-PV-7), so a
    // plane shard that never answered leaves its volumes uncovered and
    // the pass INCOMPLETE — never relocal, never narrowed silently.
    for (shard_no, peer_id) in std::mem::take(&mut plane_outstanding) {
        let owned = owner_map
            .as_ref()
            .map(|m| m.volumes_owned_by(&peer_id))
            .unwrap_or_default();
        log::warn!(
            "fsck fleet ({job_id}): inode-plane shard {shard_no} of owner '{peer_id}' never \
             answered — volumes {owned:?} are UNCOVERED for this pass"
        );
        plane_unreachable.push((peer_id, owned));
    }

    let mut merged = merge_reports(&reports);

    // ---- FINALIZE (design §4 step 6): the allocator classes + C8 over
    // the fleet-merged full census, with the existing verify ladder ----
    let mut fin_counters = FsckCounters::default();
    let mut fin_findings: Vec<FsckFinding> = Vec::new();
    if !opts.scrub_only && !opts.cancel.load(Ordering::Relaxed) {
        let mut fin_opts = opts.clone();
        fin_opts.shard = None;
        fin_opts.assume_latched = true;
        let census = match merged.partial.as_ref().filter(|p| p.mappings_complete) {
            Some(p) => CensusOut {
                refs: p.refs.clone(),
                mappings: p.mappings.clone(),
                // Unresolvable mappings were adjudicated PER SHARD (the
                // arm runs sharded); re-feeding them would double-report.
                unresolvable: Vec::new(),
                live: ino_bitmap(&ctx.meta),
                odd_nlink: HashMap::new(),
                odd_nlink_complete: true,
                complete: true,
                // Shards carry no kvmap head info; the finalize's C11 (b)
                // arm rides its own real census (`ip_census`) below.
                kvmap_empty_heads: Vec::new(),
                inodes_scanned: merged.counters.inodes_scanned,
            },
            None => {
                log::warn!(
                    "fsck fleet ({job_id}): a shard degraded its mapping list (oversize \
                     wire frame) — the finalize walks the coordinator census instead \
                     (the stated Amdahl term, design-mw-fleet-jobs §4)"
                );
                walk_census(ctx, &fin_opts, &mut fin_counters).await?
            }
        };
        let mut fin_suspects: Vec<Suspect> = Vec::new();
        evaluate_allocator_classes(
            &vols,
            &census,
            &fin_opts,
            &mut fin_counters,
            &mut fin_suspects,
        );
        evaluate_c8(ctx, &mut fin_suspects).await;
        evaluate_c16(ctx, &mut fin_suspects).await;
        // C12 over the fleet-merged mapping list — the one place a pair of
        // tenants split across two members' residues meets.
        evaluate_c12_tenant_ranges(&census, &mut fin_suspects);
        // C11 (a) rides the finalize like C8 — the orphan skip-scan is
        // self-contained, so a fleet pass never covers less than the
        // coordinator's own unsharded run. The (b) arm follows the
        // finalize's own real census (`ip_census`) below.
        evaluate_c11_orphans(ctx, &mut fin_counters, &mut fin_suspects).await;
        // C11 (c) — PR 6a (design §12): run-vs-point coverage sanity,
        // finalize-only like (a) (the same owner skip-scan feeds it).
        evaluate_c11_run_coverage(ctx, &mut fin_counters, &mut fin_suspects).await;

        // ---- The INODE PLANE, judged from ONE view PER OWNER (the
        // 2026-08-17 tarx C10 conviction, restated by KD-PV-16;
        // `FsckOptions::inode_plane`) ----
        //
        // Every CENSUS shard — member and local alike — skipped C9/C10:
        // their verdicts are census-vs-dentry-pass AGREEMENT, which only
        // means something when both walks and every verification read
        // share one authority's coherent instant. A member's S5 reader
        // view mixes per-volume checkpoint instants mid-churn and
        // manufactures the loss-direction shapes from a healthy tree (25
        // self-healing findings on the leg2 capture). What this pass adds
        // is the coordinator's OWN half — the volumes IT appends to (its
        // whole set on a single-authority mount) — while the owner shards
        // collected above cover the rest. Cost: one census + one dentry
        // pass on the coordinator, the stated Amdahl term of the fan-out
        // (design-mw-fleet-jobs §4), paid so the plane's teeth stay exact.
        // The verification ladder below (settle → witness bracket → fresh
        // pass → 4a lease re-check → intent exemption) is the unchanged
        // `recheck_suspects` machinery, shared with the allocator classes'
        // finalize.
        if !opts.cancel.load(Ordering::Relaxed) {
            let (ip_refs, ip_indexed, ip_foreign_dentry) = build_referenced_inos(
                ctx.meta.clone(),
                None,
                opts.throttle_pct,
                opts.cancel.clone(),
                None,
            )
            .await;
            fin_counters.dentry_refs_indexed += ip_indexed;
            fin_counters.inode_plane_foreign_dentry_scoped += ip_foreign_dentry;
            let ip_census = walk_census(ctx, &fin_opts, &mut fin_counters).await?;
            // C11 (b): the finalize's own unsharded census carries the
            // empty-head nominations (the merged shard census cannot —
            // shards collect no kvmap head info).
            evaluate_c11_empty_heads(&ip_census, &mut fin_counters, &mut fin_suspects, &ctx.meta);
            let frozen = !fin_opts.multi_owner || peer_volumes_are_assigned(ctx, &fin_opts).await;
            // The foreign-window scoping (Issue 12) — the same exclusion the
            // unsharded pass takes; an unreadable ring = no verdict.
            let window_inos = foreign_window_inos(ctx).await;
            match (
                ip_refs.as_ref().filter(|_| frozen && window_inos.is_some()),
                ip_census.live.truncated(),
            ) {
                (Some(refs), false) => {
                    let window_inos = window_inos.clone().unwrap_or_default();
                    evaluate_c9_unreferenced(
                        ctx,
                        &fin_opts,
                        &ip_census.live,
                        &refs.refs,
                        &window_inos,
                        &mut fin_counters,
                        &mut fin_suspects,
                    )
                    .await;
                    evaluate_c10_inode_plane(
                        ctx,
                        &fin_opts,
                        &ip_census,
                        refs,
                        &window_inos,
                        &mut fin_counters,
                        &mut fin_suspects,
                    )
                    .await;
                    for v in fin_opts.covered_volumes(ctx.meta.volumes.len()) {
                        if !plane_covered.contains(&v) {
                            plane_covered.push(v);
                        }
                    }
                }
                (Some(_), true) => log::warn!(
                    "fsck fleet ({job_id}): the live-inode set reached its derived byte \
                     budget ({} B) — the inode-plane classes record no verdict for this \
                     run",
                    ino_set_byte_budget()
                ),
                (None, _) => log::warn!(
                    "fsck fleet ({job_id}): the coordinator dentry pass did not complete \
                     — the inode-plane classes record no verdict for this run"
                ),
            }
        }
        fin_counters.suspects += fin_suspects.len() as u64;
        if !fin_suspects.is_empty() {
            squeezefs_ipc::sqz_time::sleep(opts.settle).await;
            for v in &vols {
                v.alloc.fsck_bump_epoch();
            }
            recheck_suspects(
                ctx,
                &fin_opts,
                &vols,
                fin_suspects,
                &mut fin_counters,
                &mut fin_findings,
            )
            .await?;
        }
    }
    drop(latch_release);

    merged.findings.extend(fin_findings);
    merged.findings.sort();
    merged.findings.dedup();
    fold_finalize_counters(&mut merged.counters, &fin_counters);
    merged.counters.findings = merged.findings.len() as u64;
    merged.counters.scan_secs = started.elapsed().as_secs();
    merged.shard = None;
    merged.mode = "online".to_string();

    // ---- KD-PV-16: coverage is an ASSERTION, not a claim ----
    //
    // `covered == volume_count` is half of PR 6's gate precisely because
    // the other half (`findings == 0`) would otherwise pass trivially
    // over 1/K of the inodes. The union is coordinator-derived: its own
    // plane's volumes plus, per admitted shard, the intersection of what
    // that shard reported with what THIS node's owner map says its holder
    // owns.
    plane_covered.sort_unstable();
    plane_covered.dedup();
    merged.counters.inode_plane_volumes_covered = plane_covered.len() as u64;
    merged.counters.inode_plane_proposals_admitted += plane_admitted;
    merged.counters.inode_plane_proposals_stripped += plane_stripped;
    merged.inode_plane_covered = plane_covered;
    if merged.counters.inode_plane_volumes_covered < ctx.meta.volumes.len() as u64 {
        log::warn!(
            "fsck fleet ({job_id}): the inode plane covered {} of {} volumes — this pass is \
             INCOMPLETE, not a narrower verdict (design-per-volume-claim-admission §5.8.1). \
             Covered: {:?}; unreachable owner(s): {:?}",
            merged.counters.inode_plane_volumes_covered,
            ctx.meta.volumes.len(),
            merged.inode_plane_covered,
            plane_unreachable
        );
    }

    // Publish the WORKER + FINALIZE shares on the coordinator's stats
    // inode (the local shards published themselves inside `run`): the
    // coordinator's `fsck_*` deltas account for the whole fleet pass
    // exactly once, and each member's stats carry its own share — the
    // gate-1 partition-accounting instrument. `findings` rides the
    // MERGED (deduped) count via the reports, never the per-shard sums.
    fin_counters.findings = 0;
    fin_counters.scan_secs = 0;
    publish_metrics(&worker_counters);
    publish_metrics(&fin_counters);
    fleet.retire_fleet_shards(job_id);
    crate::fuse_client::METRICS
        .fsck_scan_secs
        .store(merged.counters.scan_secs, Ordering::Relaxed);
    // The coverage gauge is the PASS's answer, so the coordinator stores
    // the composed union rather than any one shard's share (the local
    // shards published their own inside `run`, and a fleet pass's answer
    // is the union or it is nothing).
    crate::fuse_client::METRICS
        .fsck_inode_plane_volumes_covered
        .store(
            merged.counters.inode_plane_volumes_covered,
            Ordering::Relaxed,
        );

    Ok(merged)
}

/// **The §5.8.2 admission predicate** — the one place a faithful-but-wrong
/// implementation of KD-PV-16 would re-admit the `fix/mw-xv-unlink-c10`
/// mirage.
///
/// `owned` is what the **coordinator's own** `OwnerMap` says the shard's
/// lease HOLDER owns, read from the coordinator's own lease table and
/// never from the payload (clause 2). An inode-plane finding is retained
/// only when the volume the coordinator's OWN `route_ino` derives from
/// its ino is in that set — and for `C10DanglingDentry`, which carries an
/// explicit `vol` beside its `child_ino`, BOTH must be (clause 3: the
/// dentry record's removal is a commit on the name's volume).
///
/// `owned.is_empty()` — every member/reader shard, every worker the map
/// does not name, every older or foreign binary — takes the pre-KD-PV-16
/// path VERBATIM: every C9/C10 finding dropped and the six inode-plane
/// counters zeroed, so a time-shifted verdict can neither merge nor move
/// the coordinator's `fsck_nlink_zero_named`/`fsck_dangling_dentries`
/// stop-and-read tripwires. That preservation is the regression barrier:
/// a fleet worker of this binary only ever proposes the plane when the
/// coordinator asked it to as an OWNER.
///
/// Returns `(admitted, stripped)`.
fn admit_inode_plane_proposals(
    r: &mut FsckReport,
    owned: &[usize],
    meta: &RoutedMetaBackend,
) -> (usize, usize) {
    let before = r.findings.len();
    if owned.is_empty() {
        r.findings
            .retain(|f| !matches!(f.class.as_str(), "C9" | "C10"));
        let stripped = before - r.findings.len();
        let c = &mut r.counters;
        c.nlink_mismatch_high = 0;
        c.nlink_mismatch_low = 0;
        c.nlink_zero_named = 0;
        c.dangling_dentries = 0;
        c.nlink_names_counted = 0;
        c.nlink_transient_cleared = 0;
        c.current_era_exempted = 0;
        c.unreferenced_intent_exempted = 0;
        c.inode_plane_volumes_covered = 0;
        c.findings = r.findings.len() as u64;
        r.inode_plane_covered.clear();
        return (0, stripped);
    }
    // The volume is derived by the COORDINATOR's own routing — ino →
    // slot → volume through its own durable slot map — never taken from
    // the finding's text or the shard's claim.
    let owns = |ino: u64| owned.contains(&meta.route_ino(ino).0);
    let mut admitted = 0usize;
    r.findings.retain(|f| {
        if !matches!(f.class.as_str(), "C9" | "C10") {
            return true;
        }
        let keep = match &f.identity {
            Some(FindingId::C9Unreferenced { ino })
            | Some(FindingId::C10NlinkTooHigh { ino })
            | Some(FindingId::C10NlinkTooLow { ino })
            | Some(FindingId::C10ZeroNlinkNamed { ino }) => owns(*ino),
            Some(FindingId::C10DanglingDentry { vol, child_ino, .. }) => {
                owned.contains(vol) && owns(*child_ino)
            }
            // An inode-plane finding with no structured identity cannot
            // be attributed to a volume, so it cannot be admitted (repair
            // refuses it anyway — an older binary's report).
            _ => false,
        };
        admitted += usize::from(keep);
        keep
    });
    // Coverage is the INTERSECTION of what the shard reports with what
    // the coordinator's map grants it: a declaration can only narrow.
    r.inode_plane_covered.retain(|v| owned.contains(v));
    r.inode_plane_covered.sort_unstable();
    r.inode_plane_covered.dedup();
    r.counters.inode_plane_volumes_covered = r.inode_plane_covered.len() as u64;
    r.counters.findings = r.findings.len() as u64;
    (admitted, before - r.findings.len())
}

/// Sum a worker shard's census counters into the coordinator-published
/// aggregate (every field except the merge-adjudicated `findings` and
/// the per-run `scan_secs`, zeroed by the caller).
fn fold_worker_counters(dst: &mut FsckCounters, src: &FsckCounters) {
    dst.inodes_scanned += src.inodes_scanned;
    dst.nodes_walked = dst.nodes_walked.max(src.nodes_walked);
    dst.dentry_refs_indexed += src.dentry_refs_indexed;
    dst.current_era_exempted += src.current_era_exempted;
    dst.unreferenced_intent_exempted += src.unreferenced_intent_exempted;
    dst.nlink_names_counted += src.nlink_names_counted;
    dst.nlink_mismatch_high += src.nlink_mismatch_high;
    dst.nlink_mismatch_low += src.nlink_mismatch_low;
    dst.nlink_zero_named += src.nlink_zero_named;
    dst.dangling_dentries += src.dangling_dentries;
    dst.nlink_transient_cleared += src.nlink_transient_cleared;
    dst.inode_plane_foreign_scoped += src.inode_plane_foreign_scoped;
    dst.inode_plane_foreign_slot_scoped += src.inode_plane_foreign_slot_scoped;
    dst.inode_plane_foreign_dentry_scoped += src.inode_plane_foreign_dentry_scoped;
    dst.c1_projection_slots_scoped += src.c1_projection_slots_scoped;
    dst.inode_plane_window_scoped += src.inode_plane_window_scoped;
    dst.inode_plane_slots_covered += src.inode_plane_slots_covered;
    dst.inode_plane_cross_owner_declined += src.inode_plane_cross_owner_declined;
    dst.blocks_checked += src.blocks_checked;
    dst.refcounts_checked += src.refcounts_checked;
    dst.suspects += src.suspects;
    dst.suspects_cleared += src.suspects_cleared;
    dst.epoch_exempted += src.epoch_exempted;
    dst.inflight_exempted += src.inflight_exempted;
    dst.mover_ledger_exempted += src.mover_ledger_exempted;
    dst.pack_ledger_exempted += src.pack_ledger_exempted;
    dst.foreign_lane_exempted += src.foreign_lane_exempted;
    dst.alloc_bitmap_leak_candidates += src.alloc_bitmap_leak_candidates;
    dst.alloc_bitmap_tracked_exempted += src.alloc_bitmap_tracked_exempted;
    dst.tenant_overlap_findings += src.tenant_overlap_findings;
    dst.shared_index_drift += src.shared_index_drift;
    dst.stripe_findings += src.stripe_findings;
    dst.slot_custody_conflicts += src.slot_custody_conflicts;
    dst.unrecovered_appenders += src.unrecovered_appenders;
    dst.scrub_blocks_scanned += src.scrub_blocks_scanned;
    dst.scrub_bytes_scanned += src.scrub_bytes_scanned;
    dst.scrub_aead_verified += src.scrub_aead_verified;
    dst.scrub_frame_verified += src.scrub_frame_verified;
    dst.scrub_readability_only += src.scrub_readability_only;
    dst.scrub_failures += src.scrub_failures;
}

// ---------------------------------------------------------------------------
// Volume/backend resolution
// ---------------------------------------------------------------------------

/// Every distinct allocator/device pair under its canonical id
/// (the default slot and its named registration dedupe by Arc
/// identity).
fn volume_allocators(ctx: &FsckCtx) -> Vec<VolAlloc> {
    let br = &ctx.router.backend_router;
    let mut out: Vec<VolAlloc> = Vec::new();
    let mut push = |id: &str, alloc: &Arc<BlockAllocator>| {
        if !out.iter().any(|v| Arc::ptr_eq(&v.alloc, alloc)) {
            out.push(VolAlloc {
                id: id.to_string(),
                alloc: alloc.clone(),
            });
        }
    };
    for entry in br.backends.iter() {
        push(entry.key(), &entry.value().block_allocator);
    }
    push(br.default_allocator.volume_id(), &br.default_allocator);
    out
}

/// Canonicalize a parsed backend id to the [`volume_allocators`] id
/// (`backend_0`/`squeezefs` aliases resolve to the default slot).
fn canonical_backend(ctx: &FsckCtx, be_id: &str) -> Option<(String, Arc<BlockAllocator>)> {
    let br = &ctx.router.backend_router;
    if be_id == "backend_0" || be_id == "squeezefs" {
        return Some((
            br.default_allocator.volume_id().to_string(),
            br.default_allocator.clone(),
        ));
    }
    if let Some(be) = br.backends.get(be_id) {
        let alloc = be.value().block_allocator.clone();
        // Named registration of the default slot canonicalizes to the
        // default allocator's id (one identity per allocator).
        if Arc::ptr_eq(&alloc, &br.default_allocator) {
            return Some((br.default_allocator.volume_id().to_string(), alloc));
        }
        return Some((be_id.to_string(), alloc));
    }
    None
}

// ---------------------------------------------------------------------------
// C1: tree walk (checksum ride-along + semantic checks)
// ---------------------------------------------------------------------------

/// The C1 checksum-valid schema check for one record — shared by the
/// detection walk and the repair engine's verify-before-repair re-check.
fn record_schema_violation(tree_id: u8, k: &[u8], v: &[u8]) -> Option<String> {
    use crate::meta_backend::kv::record::{
        decode_dentry_key, decode_inode_key, decode_xattr_key, DentryValue, InodeValue, XattrValue,
        TREE_DENTRIES, TREE_INODES, TREE_XATTRS,
    };
    match tree_id {
        t if t == TREE_INODES => match decode_inode_key(k) {
            Err(e) => Some(format!("key does not decode as an inode key: {e}")),
            Ok(_) => InodeValue::decode(v)
                .err()
                .map(|e| format!("inode value undecodable: {e}")),
        },
        t if t == TREE_DENTRIES => match decode_dentry_key(k) {
            Err(e) => Some(format!("key does not decode as a dentry key: {e}")),
            Ok(_) => DentryValue::decode(v)
                .err()
                .map(|e| format!("dentry value undecodable: {e}")),
        },
        t if t == TREE_XATTRS => match decode_xattr_key(k) {
            Err(e) => Some(format!("key does not decode as an xattr key: {e}")),
            Ok(_) => XattrValue::decode(v)
                .err()
                .map(|e| format!("xattr value undecodable: {e}")),
        },
        _ => None,
    }
}

/// One C1 walk unit: a per-kind tree on a flat volume; ONE slot tree,
/// read RAW with every kind in its mixed leaves, on a forest (the ONE-walk
/// census for C1 — design-symmetric-metadata §5.8.5; the raw read is
/// also the only one that can SEE a key the forest codec refuses: the
/// kind-routed walks skip such a record so a census never truncates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum C1Unit {
    Kind(u8),
    Slot(crate::meta_backend::kv::record::ForestSlot),
}

/// The C1 walk units of one volume, and the count of slot trees SCOPED
/// OUT because this mount's census takes no verdict over them
/// (`SlotCoverage::unjudged_slots` — the ONE predicate the dentry pass
/// reads; symmetric PR 12b rounds 4/5): on the manager, a slot tree a
/// LIVE foreign appender leases — appended into and re-rooted by the
/// lessee under grants the manager handed out, so the manager's raw walk
/// from ITS root word read a routing loop (`root-seq` restarts to the
/// budget) and reported `C1Torn` over a healthy tree (the `sym-storm`
/// leg's round-3 red); on a MEMBER (a joined appender or a `-o ro` token
/// reader running KD-MW-16's fleet census shard) every slot tree not its
/// own — no owner word exists there, and the coordinator admits the
/// shard's C1 findings whole (Issue 24). A lessee the manager's owner
/// does not list live is PR 10's frozen-tree class and is walked. Counted
/// on `fsck_c1_projection_slots_scoped`.
async fn c1_units(kv: &crate::meta_backend::kv::backend::KvMetaBackend) -> (Vec<C1Unit>, u64) {
    if kv.symmetric_forest() {
        // A coverage read that FAILS walks nothing rather than everything
        // (Issue 24): a projected tree's loop is a false finding, an
        // unwalked tree an incomplete pass — the honest one.
        let unjudged: std::collections::BTreeSet<_> = match kv.inode_plane_slot_coverage().await {
            Ok(cov) => cov.unjudged_slots.into_iter().collect(),
            Err(e) => {
                log::warn!(
                    "fsck C1: meta volume {}'s slot coverage could not be read ({e}) — no slot \
                     tree is walked this run (an incomplete pass, never a finding over a \
                     projection)",
                    kv.device_path().display()
                );
                return (Vec::new(), kv.forest_roots().len() as u64);
            }
        };
        let mut units = Vec::new();
        let mut scoped = 0u64;
        for (slot, _) in kv.forest_roots() {
            if unjudged.contains(&slot) {
                scoped += 1;
            } else {
                units.push(C1Unit::Slot(slot));
            }
        }
        if scoped > 0 {
            log::info!(
                "fsck C1: meta volume {}: {scoped} slot tree(s) are PROJECTIONS here (another \
                 appender's, or the manager's at a member) — skipped, the pass INCOMPLETE over \
                 them (fsck_c1_projection_slots_scoped)",
                kv.device_path().display()
            );
        }
        (units, scoped)
    } else {
        (
            crate::meta_backend::kv::backend::KvMetaBackend::USER_KINDS
                .into_iter()
                .map(C1Unit::Kind)
                .collect(),
            0,
        )
    }
}

/// One page of a C1 unit from `cursor` — the scan's read and its
/// re-check's (same shape: a `max = 1` probe could be satisfied by a
/// healthy left sibling and never touch the damaged node).
async fn c1_page(
    kv: &crate::meta_backend::kv::backend::KvMetaBackend,
    unit: C1Unit,
    cursor: &[u8],
) -> std::result::Result<Vec<(bytes::Bytes, bytes::Bytes)>, crate::meta_backend::kv::KvError> {
    let end = crate::meta_backend::kv::tree::KEY_SPACE_MAX;
    match unit {
        C1Unit::Kind(kind) => kv.range_kind(kind, cursor, &end, SCAN_PAGE).await,
        C1Unit::Slot(slot) => kv.slot_tree_range_raw(slot, cursor, &end, SCAN_PAGE).await,
    }
}

/// One C1 walk — the parallel unit (VL10, G-VL-5(c)): checksum
/// ride-along via the range read, cross-page key ordering, and the schema
/// check per record. On a forest the unit is a slot tree read raw: each
/// record's key is split by the codec first — a key it refuses is its own
/// suspect class (`C1RawKey`), a key it accepts is schema-checked under
/// its kind exactly like a flat volume's record. Returns `(pages_walked,
/// suspects)`.
async fn walk_one_tree_c1(
    kv: Arc<crate::meta_backend::kv::backend::KvMetaBackend>,
    vol_idx: usize,
    unit: C1Unit,
    throttle_pct: u32,
    cancel: Arc<AtomicBool>,
) -> (u64, Vec<Suspect>) {
    let mut nodes_walked = 0u64;
    let mut suspects = Vec::new();
    let mut cursor: Vec<u8> = vec![0u8];
    let mut prev_key: Option<Vec<u8>> = None;
    let (walk_tree, walk_slot) = match unit {
        C1Unit::Kind(kind) => (kind, None),
        C1Unit::Slot(slot) => (crate::meta_backend::kv::record::KIND_INTERIOR, Some(slot)),
    };
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let t0 = std::time::Instant::now();
        let page = match c1_page(&kv, unit, &cursor).await {
            Ok(p) => p,
            Err(e) => {
                suspects.push(Suspect {
                    kind: SuspectKind::C1Walk {
                        vol: vol_idx,
                        tree: walk_tree,
                        slot: walk_slot,
                        cursor: cursor.clone(),
                        error: e.to_string(),
                    },
                });
                break; // the walk cannot advance past an unreadable node
            }
        };
        nodes_walked += 1;
        let Some((last_key, _)) = page.last() else {
            break;
        };
        cursor = crate::meta_backend::kv::node::key_successor(last_key);
        for (k, v) in &page {
            // The record's kind and legacy key: as read on a flat volume;
            // split by the forest codec on a slot tree.
            let (tree_id, legacy): (u8, std::borrow::Cow<'_, [u8]>) = match unit {
                C1Unit::Kind(kind) => (kind, std::borrow::Cow::Borrowed(k.as_ref())),
                C1Unit::Slot(slot) => match crate::meta_backend::kv::record::split_forest_key(k) {
                    Ok((kind, legacy)) => (kind, std::borrow::Cow::Owned(legacy)),
                    Err(e) => {
                        suspects.push(Suspect {
                            kind: SuspectKind::C1RawKey {
                                vol: vol_idx,
                                slot,
                                key: k.to_vec(),
                                why: format!("slot-tree key refused by the forest codec: {e}"),
                            },
                        });
                        prev_key = Some(k.to_vec());
                        continue;
                    }
                },
            };
            // In-page + cross-page ordering (checksum-valid
            // structural damage surfaces here) — on the stored key.
            if let Some(prev) = &prev_key {
                if k.as_ref() <= prev.as_slice() {
                    suspects.push(Suspect {
                        kind: SuspectKind::C1Record {
                            vol: vol_idx,
                            tree: tree_id,
                            key: legacy.to_vec(),
                            why: "key ordering violated".to_string(),
                        },
                    });
                }
            }
            prev_key = Some(k.to_vec());
            let why = record_schema_violation(tree_id, &legacy, v);
            if let Some(why) = why {
                suspects.push(Suspect {
                    kind: SuspectKind::C1Record {
                        vol: vol_idx,
                        tree: tree_id,
                        key: legacy.to_vec(),
                        why,
                    },
                });
            }
        }
        throttle_sleep(throttle_pct, t0.elapsed()).await;
    }
    (nodes_walked, suspects)
}

/// The serial C1 walk — the THROTTLED shape (KD-3's duty cycle is per
/// worker; the unthrottled parallel shape lives in [`run`]'s pass 1).
async fn walk_trees_c1(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    counters: &mut FsckCounters,
    suspects: &mut Vec<Suspect>,
) {
    for (vol_idx, kv) in ctx.meta.volumes.iter().enumerate() {
        let (vol_units, scoped) = c1_units(kv).await;
        counters.c1_projection_slots_scoped += scoped;
        for unit in vol_units {
            if opts.cancel.load(Ordering::Relaxed) {
                return;
            }
            let (nodes_walked, walk_suspects) = walk_one_tree_c1(
                kv.clone(),
                vol_idx,
                unit,
                opts.throttle_pct,
                opts.cancel.clone(),
            )
            .await;
            counters.nodes_walked += nodes_walked;
            suspects.extend(walk_suspects);
        }
    }
}

// ---------------------------------------------------------------------------
// Census walk (the `df` walk shape: inode tree pages + layout xattrs)
// ---------------------------------------------------------------------------

async fn walk_census(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    counters: &mut FsckCounters,
) -> Result<CensusOut> {
    use crate::meta_backend::kv::record::{decode_inode_key, inode_key, InodeValue};
    let block_size = ctx
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let mut out = CensusOut {
        live: ino_bitmap(&ctx.meta),
        ..probe_census()
    };
    let odd_budget = c10_count_entry_budget();
    for (vol_idx, kv) in ctx.meta.volumes.iter().enumerate() {
        let mut cursor: Vec<u8> = inode_key(1).to_vec();
        let end = inode_key(u64::MAX - 1);
        loop {
            if opts.cancel.load(Ordering::Relaxed) {
                out.complete = false;
                break;
            }
            let t0 = std::time::Instant::now();
            let page = match kv
                .range_kind(
                    crate::meta_backend::kv::record::TREE_INODES,
                    &cursor,
                    &end,
                    SCAN_PAGE,
                )
                .await
            {
                Ok(p) => p,
                // The C1 walk owns reporting unreadable nodes; the census
                // takes what it can reach — and says so, because C10's
                // reverse arms would read the gap as missing records.
                Err(_) => {
                    out.complete = false;
                    break;
                }
            };
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = crate::meta_backend::kv::node::key_successor(last_key);
            for (k, v) in &page {
                let Ok(local_ino) = decode_inode_key(k) else {
                    continue; // C1's business
                };
                let Ok(val) = InodeValue::decode(v) else {
                    continue;
                };
                // A `nlink == 0` corpse (unlinked, its kernel FORGET not
                // yet arrived, reclaim not yet run) is LEGAL live state
                // that still OWNS its blocks: it stays OUT of every
                // inode-plane set below (C9's live bitmap, C10's count
                // arms — their `nlink == 0` exclusions are deliberate and
                // documented) but its layout DOES feed the block census.
                // The old whole-record skip read every such block as C2
                // "leaked" and its ledger record as C8 drift — 152 false
                // positives on a healthy fleet seconds after a tar-x/rm
                // pass (2026-08-23, the corpse-census correction).
                let corpse = val.nlink == 0;
                // Guest-only members carry raw CONTROL records with no
                // global encoding — skip them (VL9 soak-found panic).
                let Some(global_ino) = ctx.meta.try_make_global_ino(local_ino, vol_idx) else {
                    continue;
                };
                if let Some((shard_k, shard_n)) = opts.shard {
                    if global_ino % shard_n as u64 != shard_k as u64 {
                        continue;
                    }
                }
                if !corpse {
                    out.inodes_scanned += 1;
                    // C9: this live inode's bit. One bit per visited
                    // inode, on a walk that already reads every inode
                    // record — the set difference happens after the
                    // (independent) dentry pass, so nothing here depends
                    // on scan order.
                    out.live.mark(global_ino);
                    // C10: the count arms' small side — non-directory
                    // live inodes whose nlink is not 1. Directories are
                    // excluded BY CONSTRUCTION (their nlink counts `.`
                    // and every child's `..`, which are synthesized and
                    // never records), which is also what keeps this map
                    // the hardlink population instead of the whole tree.
                    if val.nlink != 1 && val.mode & libc::S_IFMT != libc::S_IFDIR {
                        if out.odd_nlink.len() as u64 >= odd_budget {
                            out.odd_nlink_complete = false;
                        } else {
                            out.odd_nlink.insert(global_ino, val.nlink);
                        }
                    }
                }
                // KD-PV-16's plane shard has no use for the block map —
                // and the per-inode `layout` read is what a census
                // actually costs, so skipping it is what makes an owner
                // shard cheap enough to run beside the residue partition.
                if opts.inode_plane_only {
                    continue;
                }
                let Ok(Some(bytes)) = kv.getxattr(local_ino, "layout").await else {
                    continue;
                };
                let layout: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{") {
                    serde_json::from_slice(&bytes).ok()
                } else {
                    bincode::deserialize(&bytes).ok()
                };
                let Some(layout) = layout else { continue };
                // PR 2 (kvmap, Rev 1.1 #3): a `kvmap:` head's entries are
                // paged out of tree 7 through the SHARED extraction —
                // the derived census's own arm — so both C8 oracle sides
                // read the same multiset and a healthy kvmap volume
                // reads zero drift.
                let tree_entries = kvmap_entries_for(ctx, kv, local_ino, &layout).await;
                // PR 4 — C11 (b) nomination (design §3 fsck): a kvmap
                // head with nonzero size and ZERO tree records. Corpses
                // never nominate (their sweep runs at reclaim), and a
                // head with an open sweep cursor is exempt-in-range (A2).
                if !corpse && layout.size > 0 {
                    if let Some(entries) = &tree_entries {
                        if entries.is_empty()
                            && layout.block_map_id.as_deref().is_some_and(|id| {
                                crate::meta_backend::kv::block_map::parse_kvmap_head(id)
                                    .is_ok_and(|h| h.sweep_cursor.is_none())
                            })
                        {
                            out.kvmap_empty_heads
                                .push((vol_idx, local_ino, layout.size));
                        }
                    }
                }
                census_layout(ctx, global_ino, &layout, block_size, tree_entries, &mut out).await;
            }
            throttle(opts, t0.elapsed()).await;
        }
    }
    counters.blocks_checked = out
        .refs
        .values()
        .map(|m| m.len() as u64)
        .sum::<u64>()
        .max(counters.blocks_checked);
    Ok(out)
}

/// PR 2 (kvmap): the census's tree-entry input for one layout — a
/// `kvmap:` head's records through the SHARED extraction
/// (`BackendRouter::kvmap_layout_entries`, Rev 1.1 #3: both C8 oracle
/// sides read the same multiset), `None` for every other head.
async fn kvmap_entries_for(
    ctx: &FsckCtx,
    kv: &crate::meta_backend::kv::backend::KvMetaBackend,
    local_ino: u64,
    layout: &crate::routing::LayoutMetadata,
) -> Option<Vec<(u32, String)>> {
    if layout
        .block_map_id
        .as_deref()
        .is_some_and(|id| id.starts_with(crate::meta_backend::kv::block_map::KVMAP_HEAD_PREFIX))
    {
        Some(
            ctx.router
                .backend_router
                .kvmap_layout_entries(kv, local_ino)
                .await,
        )
    } else {
        None
    }
}

/// One layout's contribution: block-map entries (inline, indirect — the
/// blob block itself included — or `tree_entries`, a kvmap head's
/// records pre-paged through the SHARED extraction) — mirrors
/// `recover_active_blocks_v3`'s accounting exactly.
async fn census_layout(
    ctx: &FsckCtx,
    global_ino: u64,
    layout: &crate::routing::LayoutMetadata,
    block_size: usize,
    tree_entries: Option<Vec<(u32, String)>>,
    out: &mut CensusOut,
) {
    let count_ref = |mapping: &str, ino: u64, idx: u32, out: &mut CensusOut| {
        // §5.6a quarantined mapping: the reference is COUNTED (the
        // physical block is intentionally preserved and must stay
        // refcount-coherent), but the mapping is flagged so the scrub
        // and the lost checks skip it — it is the repair, not damage.
        let damaged = crate::routing::is_damaged_mapping(mapping);
        let clean = clean_key(mapping);
        match ctx.router.backend_router.parse_block_key(&clean) {
            Ok((be_id, offset)) => match canonical_backend(ctx, &be_id) {
                Some((vol, _)) => {
                    *out.refs
                        .entry(vol.clone())
                        .or_default()
                        .entry(offset)
                        .or_insert(0) += 1;
                    out.mappings.push(MappingRef {
                        ino,
                        block_idx: idx,
                        mapping: mapping.to_string(),
                        vol,
                        offset,
                        damaged,
                    });
                }
                None if damaged => {} // quarantined AND unresolvable: already isolated
                None => out.unresolvable.push(MappingRef {
                    ino,
                    block_idx: idx,
                    mapping: mapping.to_string(),
                    vol: "?".to_string(),
                    offset: 0,
                    damaged,
                }),
            },
            Err(_) if damaged => {}
            Err(_) => out.unresolvable.push(MappingRef {
                ino,
                block_idx: idx,
                mapping: mapping.to_string(),
                vol: "?".to_string(),
                offset: 0,
                damaged,
            }),
        }
    };

    let mut entries: Vec<(u32, String)> = tree_entries.unwrap_or_default();
    if let Some(ref map_id) = layout.block_map_id {
        if let Some(blob_key) = map_id.strip_prefix("indirect:") {
            // The blob block itself is a referenced block.
            count_ref(blob_key, global_ino, u32::MAX, out);
            if let Ok(raw) = ctx
                .router
                .backend_router
                .read_block(blob_key, block_size)
                .await
            {
                if let Ok(decoded) = crate::routing::decode_indirect_block_map(&raw) {
                    entries = decoded;
                }
            }
        }
    }
    if entries.is_empty() {
        if let Some(ref bm) = layout.block_map {
            entries = bm.iter().map(|(&b, key)| (b, key.clone())).collect();
        }
    }
    for (b, mapping) in entries {
        count_ref(&mapping, global_ino, b, out);
    }
}

/// Strip decoration to the clean base key (`proto://offset` / `offset`).
/// A §5.6a `damaged:` quarantine marker strips to its preserved BASE key.
fn clean_key(mapping: &str) -> String {
    let mapping = mapping
        .strip_prefix(crate::routing::DAMAGED_MAPPING_PREFIX)
        .unwrap_or(mapping);
    if let Some(pos) = mapping.find("://") {
        let proto = &mapping[..pos];
        let rest = &mapping[pos + 3..];
        let offset = rest.split(':').next().unwrap_or(rest);
        format!("{proto}://{offset}")
    } else {
        mapping.split(':').next().unwrap_or(mapping).to_string()
    }
}

// ---------------------------------------------------------------------------
// C9 / C10: the referenced-ino pass + the set differences
// ---------------------------------------------------------------------------

/// One dentry record naming a C10 candidate — the exact-count unit and the
/// dangling arm's repair identity.
#[derive(Clone, Debug)]
struct NameRef {
    vol: usize,
    /// The dentry record's EXACT key (a collision chain holds several keys
    /// for one name, so the key is the identity and the name is display).
    key: Vec<u8>,
    /// GLOBAL parent ino — what makes the distinct-name count stable
    /// across a VL5b slot migration (local parents differ per keyspace,
    /// global inos are eternal).
    parent: u64,
    name: Vec<u8>,
    file_type: u8,
}

/// What ONE dentry pass yields: C9's referenced-ino set, plus C10's record
/// counts for the inos named more than once, plus (when the caller already
/// knows which inos it is judging) those inos' deduped name identities.
struct RefPass {
    refs: InoBitmap,
    /// Global ino → dentry RECORDS naming it, for inos with more than one.
    /// Records, NOT distinct paths: the exact distinct count comes from
    /// [`Self::names`], so this may only nominate candidates.
    multi: HashMap<u64, u32>,
    /// `false` ⇔ [`Self::multi`] hit [`c10_count_entry_budget`]: the C10
    /// COUNT arms then record no verdict (a dropped entry would read as one
    /// name and invert a comparison). C9 and C10's DANGEROUS arms are
    /// unaffected — they read only `refs`.
    multi_complete: bool,
    /// Global ino → its distinct names, for the inos in the caller's
    /// collect set. Empty when nothing was collected.
    names: HashMap<u64, Vec<NameRef>>,
    /// C17 (PR 7b, design §5.6.5): every stripe-map MARKER the pass saw —
    /// `(volume, local parent, marker, child)` — bounded by `K + 2` per
    /// striped directory. The ONE dentry walk carries the map census; the
    /// class reads the striped population alone from it.
    markers: Vec<(usize, u64, crate::meta_backend::dir_stripe::Marker, u64)>,
}

impl RefPass {
    /// Names referencing `ino` as the RECORD count (≥ the distinct count):
    /// the map when it has more than one, else 1 iff the bit is set, else 0
    /// (C9's object). Nomination input only.
    fn record_names_of(&self, ino: u64) -> u32 {
        match self.multi.get(&ino) {
            Some(n) => *n,
            None => u32::from(self.refs.contains(ino)),
        }
    }

    /// The DISTINCT names collected for `ino` — every C10 verdict's input.
    fn collected(&self, ino: u64) -> &[NameRef] {
        self.names.get(&ino).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

/// ONE sequential pass over `TREE_DENTRIES` on every volume, marking the
/// TARGET ino of every dentry record — `DentryValue::child_ino`, which is
/// the GLOBAL ino (a dentry lives on its PARENT's volume and may name a
/// child on another, so the union across volumes is the referenced set).
/// Spawnable: it touches only the dentry trees, disjoint from the census
/// and the C1 walks.
///
/// Sharded runs mark only `child_ino % n == k` (the module header's shard
/// rule: the dentry's VALUE says which inode is named; its key's parent
/// ino says nothing).
///
/// Returns `None` when the pass could not complete (an unreadable node —
/// C1's business to report — or cancellation). A TRUNCATED referenced set
/// would make every inode named beyond the tear look unreferenced, so C9
/// records **no verdict** for that run rather than guessing; the same
/// posture C8 takes when its comparison fails.
///
/// **C10 rides the same walk.** `multi` counts the dentry RECORDS naming
/// each ino that has more than one ([`InoBitmap::mark`]'s return value is
/// what separates the first name from the rest, so the single-name majority
/// costs nothing), and `collect` — when the caller already knows which inos
/// it is judging — gathers those inos' name identities in the SAME pass.
/// A record count is deliberately an over-estimate of the DISTINCT name
/// count (a VL5b slot migration mid-copy has one dentry on two volumes), so
/// it may only NOMINATE a candidate; every C10 verdict is taken from the
/// deduped `(global parent, name)` identities `collect` returns.
async fn build_referenced_inos(
    meta: Arc<RoutedMetaBackend>,
    shard: Option<(u32, u32)>,
    throttle_pct: u32,
    cancel: Arc<AtomicBool>,
    collect: Option<&std::collections::HashSet<u64>>,
) -> (Option<RefPass>, u64, u64) {
    use crate::meta_backend::kv::record::{decode_dentry_key, DentryValue};
    let mut pass = RefPass {
        refs: ino_bitmap(&meta),
        multi: HashMap::new(),
        multi_complete: true,
        names: HashMap::new(),
        markers: Vec::new(),
    };
    let budget = c10_count_entry_budget();
    let mut indexed = 0u64;
    let mut foreign_dentry_scoped = 0u64;
    for (vol_idx, kv) in meta.volumes.iter().enumerate() {
        // A slot a LIVE foreign appender leases (symmetric PR 12b, review
        // round 1, Issue 1): its dentries live in the LESSEE's tree, which
        // this mount holds only as a projection — the images it flushed at
        // the last transfer or recovery, appended into by the lessee under
        // an unchanged root (KD-SYM-5: a non-lessee's cache of a leased
        // slot is not authoritative) — so a dentry read here may be
        // removals behind, and every such name whose child slot this
        // mount DOES judge reads as a dangling dentry (the `sym-storm`
        // leg: a remounted joiner's 1,444 unlinks, its 40 LRU-released
        // child slots re-read fresh, 442 false C10 findings). PR 8's
        // lessee-shard law scoped the INODE side only; the NAME side is
        // scoped here: the dentry set is complete on this mount only when
        // no slot of the volume is leased to a LIVE peer (the installed S6
        // owner's word — `SlotCoverage::foreign_live`); a lessee NOT known
        // live (dead-and-unrecorded, expired, or no plane) is PR 10's
        // class — its tree frozen at its page root, its window scoped out
        // (`foreign_window_inos`), the fleet judged. Reading a live
        // lessee's dentries through its holder (a census verb on the S8
        // wire) is the instrument that restores the verdict with joiners
        // live — PR 13's; until it lands the plane records NO verdict,
        // counted (`fsck_inode_plane_foreign_dentry_scoped`), never a
        // finding over a tree whose staleness it cannot bound.
        match kv.inode_plane_slot_coverage().await {
            Ok(cov) if !cov.unjudged_slots.is_empty() => {
                foreign_dentry_scoped += 1;
                log::warn!(
                    "fsck C9/C10: meta volume {vol_idx} has {} slot(s) whose trees are \
                     PROJECTIONS here (another LIVE appender's, or every non-own tree at a \
                     member) — their dentries are the lessee's, held here at a staleness the \
                     census cannot bound, so the referenced-ino set is not this mount's to \
                     census and the inode-plane classes record NO verdict this run \
                     (fsck_inode_plane_foreign_dentry_scoped; the lessee's clean leave or its \
                     recovery makes the set this mount's again)",
                    cov.unjudged_slots.len()
                );
                return (None, indexed, foreign_dentry_scoped);
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!(
                    "fsck C9/C10: meta volume {vol_idx}'s slot coverage could not be read \
                     ({e}) — the inode-plane classes record no verdict this run"
                );
                return (None, indexed, foreign_dentry_scoped);
            }
        }
        // A forest volume's dentry set is COMPLETE on a non-writer only
        // when no appender page is `Live` (symmetric PR 7b review round 1,
        // Issue 21b): a `Live` page at a probe's open is a dead writer's
        // (or a declared region's) UNCOVERED ring window — acked records
        // durable in that ring, replayed by a WRITER's own-residue open
        // and by nothing else — so a census over the trees alone is not a
        // census. The inode plane records no verdict; the writer's next
        // clean open (or PR 10's recovery) is what completes the set.
        if kv.is_read_only() || kv.filters_unpublished_children() {
            if let Some(live) = kv
                .appender_stats()
                .map(|s| s.live_pages_at_mount)
                .filter(|n| *n > 0)
            {
                log::warn!(
                    "fsck C9/C10/C17: meta volume {vol_idx} carries {live} LIVE appender \
                     page(s) this read-only probe did not replay — its acked records may sit \
                     in an uncovered ring window, so the referenced-ino set is incomplete and \
                     the inode-plane classes record no verdict for this run (a writer's open \
                     replays its own residue; a dead peer's is PR 10's recovery)"
                );
                return (None, indexed, foreign_dentry_scoped);
            }
        }
        let mut cursor: Vec<u8> = vec![0u8];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return (None, indexed, foreign_dentry_scoped);
            }
            let t0 = std::time::Instant::now();
            let page = match kv
                .range_kind(
                    crate::meta_backend::kv::record::TREE_DENTRIES,
                    &cursor,
                    &crate::meta_backend::kv::tree::KEY_SPACE_MAX,
                    SCAN_PAGE,
                )
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    log::warn!(
                        "fsck C9/C10: dentry walk of volume {vol_idx} failed ({e}) — the \
                         referenced-ino set is incomplete, so the inode-plane classes \
                         record no verdict for this run (C1 owns the unreadable node)"
                    );
                    return (None, indexed, foreign_dentry_scoped);
                }
            };
            let Some((last, _)) = page.last() else { break };
            cursor = crate::meta_backend::kv::node::key_successor(last);
            for (k, v) in &page {
                let Ok(d) = DentryValue::decode(v) else {
                    continue; // C1's business
                };
                // C17's census rides this walk: a NUL-led name is a
                // stripe-map marker (no user name can start with NUL).
                if let Some(m) = crate::meta_backend::dir_stripe::parse_marker(&d.name) {
                    if let Ok((local_parent, _, _)) = decode_dentry_key(k) {
                        pass.markers.push((vol_idx, local_parent, m, d.child_ino));
                    }
                }
                if let Some((k, n)) = shard {
                    if d.child_ino % n as u64 != k as u64 {
                        continue;
                    }
                }
                let first_name = pass.refs.mark(d.child_ino);
                indexed += 1;
                // C10: the second-and-later names, counted only for inos
                // whose FIRST name marked a bit — an out-of-range/hostile
                // child ino marks nothing and is never counted (C9's "a
                // dentry value is never an allocation authority" law).
                if !first_name && pass.refs.contains(d.child_ino) {
                    let at_budget = pass.multi.len() as u64 >= budget;
                    match pass.multi.get_mut(&d.child_ino) {
                        Some(n) => *n += 1,
                        None if at_budget => pass.multi_complete = false,
                        None => {
                            pass.multi.insert(d.child_ino, 2);
                        }
                    }
                }
                // C10's verdict input: the identities of the names of the
                // inos this run is judging, deduped by (global parent,
                // name) as the pass sees them.
                if collect.is_some_and(|want| want.contains(&d.child_ino)) {
                    let parent = decode_dentry_key(k)
                        .ok()
                        .and_then(|(local_parent, _, _)| {
                            meta.try_make_global_ino(local_parent, vol_idx)
                        })
                        .unwrap_or(0);
                    let entry = pass.names.entry(d.child_ino).or_default();
                    if !entry.iter().any(|n| n.parent == parent && n.name == d.name)
                        && (entry.len() as u64) < budget
                    {
                        entry.push(NameRef {
                            vol: vol_idx,
                            key: k.to_vec(),
                            parent,
                            name: d.name.clone(),
                            file_type: d.file_type,
                        });
                    }
                }
            }
            throttle_sleep(throttle_pct, t0.elapsed()).await;
            if pass.refs.truncated() {
                log::warn!(
                    "fsck C9/C10: the referenced-ino set reached its derived byte budget \
                     ({} B) — the inode-plane classes record no verdict for this run (a \
                     partial set would report named inodes as unreferenced)",
                    ino_set_byte_budget()
                );
                return (None, indexed, foreign_dentry_scoped);
            }
        }
    }
    (Some(pass), indexed, foreign_dentry_scoped)
}

/// The C9 difference: live inodes with no dentry, filtered to those
/// minted in a PRIOR writer era (the live-create shield — module header),
/// with the counted evidence read per candidate. Per-candidate I/O only,
/// so a healthy volume pays exactly the bitmap scan.
async fn evaluate_c9_unreferenced(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    live: &InoBitmap,
    refs: &InoBitmap,
    window_inos: &std::collections::BTreeSet<u64>,
    counters: &mut FsckCounters,
    suspects: &mut Vec<Suspect>,
) {
    // The candidate list is O(damage), like every other class's suspect
    // list — but unlike theirs its worst case is not bounded by real
    // allocator state: a volume whose dentry tree is structurally lost
    // would make EVERY inode a candidate. So the difference collects at
    // most as many inos as ONE ino set represents (`budget / 8` entries,
    // the same memory scale the sets already ride) and says loudly how
    // many it left. Batching is convergent: repair the reported ones and
    // re-run for the next batch — strictly better than either refusing a
    // verdict on a genuinely damaged volume or materializing a report
    // with a hundred million entries.
    let ceiling = (ino_set_byte_budget() / 8) as usize;
    let mut candidates: Vec<u64> = Vec::new();
    let mut deferred = 0u64;
    live.each_absent_from(refs, |ino| {
        if candidates.len() >= ceiling {
            deferred += 1;
            return;
        }
        candidates.push(ino);
    });
    if deferred > 0 {
        log::warn!(
            "fsck C9: {} unreferenced-inode candidates examined this run, {deferred}              deferred to the next run (this volume's damage exceeds one batch — repair              what is reported and re-run; the class converges)",
            candidates.len()
        );
    }
    let mut intents: Option<std::collections::HashSet<u64>> = None;
    for ino in candidates {
        let (vol_idx, local) = ctx.meta.route_ino(ino);
        let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
            continue;
        };
        // KD-PV-7: an ino homed on a volume this node does not append to
        // is that volume's OWNER's candidate, never this pass's — the
        // era floor here is a snapshot of a cursor another node advances,
        // so a record the owner minted after this mount's open would read
        // as prior-era residue. Counted, so the division of labour is
        // visible rather than a silent narrowing.
        if !opts.owns_volume(vol_idx) {
            counters.inode_plane_foreign_scoped += 1;
            continue;
        }
        // PR 8: the LESSEE shard — a slot another appender leases is its
        // lessee's candidate (KD-SYM-5's writer-cacher law read by fsck).
        if !kv.inode_plane_owns_slot(local) {
            counters.inode_plane_foreign_slot_scoped += 1;
            continue;
        }
        // Symmetric PR 10 (review round 2, Issue 12): an ino a FOREIGN
        // appender's un-replayed ring window names is in flight at its
        // holder — its verdict is the next pass's, after the window is
        // checkpointed or recovered; every other ino is judged.
        if window_inos.contains(&ino) {
            counters.inode_plane_window_scoped += 1;
            continue;
        }
        // THE guard: an inode this mount minted is never a candidate,
        // because a create legitimately holds its record before its name.
        if !kv.minted_in_prior_era(local) {
            counters.current_era_exempted += 1;
            continue;
        }
        // The inode plane's in-flight exemption (PR 13e review round 2,
        // Issue 10 — C10's `open_intent_inos`, read by C9 too): a
        // cross-owner create commits the child's record before its
        // `InsertDentry` ships, and a ship the holder refuses leaves the
        // intent OPEN for the roll-forward; the child's slot reads UNLEASED
        // once its lessee releases or leaves, so tree 0 makes the record a
        // prior-era candidate with no name — the roll-forward's object,
        // never C9's, while the intent stands (a `--repair` here would
        // destroy the record the roll-forward re-names). Read lazily: a
        // healthy volume reaches this line for no candidate.
        if intents.is_none() {
            intents = Some(open_intent_inos(ctx).await);
        }
        if intents.as_ref().is_some_and(|set| set.contains(&ino)) {
            counters.unreferenced_intent_exempted += 1;
            continue;
        }
        let val = match kv.read_inode_value_routed(local).await {
            Ok(Some(val)) => val,
            other => {
                // Vanished between the walk and here: no verdict.
                log::debug!(
                    "fsck C9: candidate ino {ino} (local {local}) read {other:?} — no verdict"
                );
                continue;
            }
        };
        // `nlink == 0` is the unlinked-but-open / POSIX-15 shape and is
        // deliberately out of scope (module header).
        if val.nlink == 0 {
            log::debug!("fsck C9: candidate ino {ino} (local {local}) is nlink 0 — out of scope");
            continue;
        }
        let blocks = layout_mappings_of(ctx, ino).await.len();
        suspects.push(Suspect {
            kind: SuspectKind::C9Unreferenced {
                ino,
                nlink: val.nlink,
                size: val.size,
                blocks,
            },
        });
    }
}

/// C10's **nomination** pass: it forms suspects and no verdicts. Every
/// number here is either an over-estimate (dentry RECORDS, which a VL5b
/// migration mid-copy inflates) or a snapshot the fresh pass will re-take,
/// so `recheck_suspects` re-derives each verdict from deduped name
/// identities under the ino's exclusive 4a lease.
///
/// Costs on a healthy volume: the count arms iterate two maps that are
/// EMPTY on a tree without hardlinks, and the reverse difference is one
/// word-parallel `AND NOT` scan (C9's, in the other direction). Per-inode
/// reads happen only for inos the cheap comparison already disagrees about.
async fn evaluate_c10_inode_plane(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    census: &CensusOut,
    pass: &RefPass,
    window_inos: &std::collections::BTreeSet<u64>,
    counters: &mut FsckCounters,
    suspects: &mut Vec<Suspect>,
) {
    counters.nlink_names_counted += pass.multi.len() as u64;

    // ---- the count arms: nlink vs the names, non-directories ----
    if pass.multi_complete && census.odd_nlink_complete {
        let mut nominated: Vec<(u64, u32, u32)> = Vec::new();
        // Side A — live non-directory inodes whose nlink is not 1.
        for (&ino, &nlink) in &census.odd_nlink {
            // KD-PV-7: candidates are scoped to the volumes this node
            // appends to (its own records are authoritative; a peer's are
            // a projection). The NAME side stays whole-set — that is the
            // §5.8.0 asymmetry.
            let (a_vol, a_local) = ctx.meta.route_ino(ino);
            if !opts.owns_volume(a_vol) {
                counters.inode_plane_foreign_scoped += 1;
                continue;
            }
            // PR 8: the lessee shard (see C9's gate).
            if ctx
                .meta
                .volumes
                .get(a_vol)
                .is_some_and(|kv| !kv.inode_plane_owns_slot(a_local))
            {
                counters.inode_plane_foreign_slot_scoped += 1;
                continue;
            }
            if window_inos.contains(&ino) {
                counters.inode_plane_window_scoped += 1;
                continue;
            }
            let records = pass.record_names_of(ino);
            // No name at all is C9's object, never C10's (reporting both
            // would double-claim one inode).
            if records > 0 && records != nlink {
                nominated.push((ino, nlink, records));
            }
        }
        // Side B — inos with more than one dentry record that side A did
        // not already judge: live, so either `nlink == 1` (a genuine
        // below-count nomination) or a directory (never a count finding).
        // One read settles which; this list is the hardlink-and-damage
        // population, not the tree.
        for (&ino, &records) in &pass.multi {
            if census.odd_nlink.contains_key(&ino) || !census.live.contains(ino) {
                continue;
            }
            let (vol_idx, local) = ctx.meta.route_ino(ino);
            if !opts.owns_volume(vol_idx) {
                counters.inode_plane_foreign_scoped += 1;
                continue;
            }
            let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
                continue;
            };
            // PR 8: the lessee shard (see C9's gate).
            if !kv.inode_plane_owns_slot(local) {
                counters.inode_plane_foreign_slot_scoped += 1;
                continue;
            }
            if window_inos.contains(&ino) {
                counters.inode_plane_window_scoped += 1;
                continue;
            }
            let Ok(Some(val)) = kv.read_inode_value_routed(local).await else {
                continue;
            };
            if val.mode & libc::S_IFMT == libc::S_IFDIR || val.nlink == 0 {
                continue;
            }
            if records != val.nlink {
                nominated.push((ino, val.nlink, records));
            }
        }
        nominated.sort_unstable();
        for (ino, nlink, names) in nominated {
            suspects.push(Suspect {
                kind: SuspectKind::C10NlinkMismatch { ino, nlink, names },
            });
        }
    } else {
        log::warn!(
            "fsck C10: the name-count maps reached their derived entry budget ({} \
             entries) — the nlink-vs-names arms record no verdict for this run (a \
             dropped entry would invert a comparison). The DANGEROUS arms (a live name \
             with nlink 0, a name resolving to nothing) are unaffected: they read only \
             the bitmaps",
            c10_count_entry_budget()
        );
    }

    // ---- the dangerous arms: the REVERSE difference (named, not live) ----
    //
    // The census skips `nlink == 0` records, so `referenced \ live` is
    // exactly {an inode whose count is 0 while a path still resolves} ∪ {a
    // name whose inode does not exist} ∪ {the census/pass concurrency
    // window}, and one fresh record read per candidate splits them. A
    // census that could not finish would put healthy inodes in that
    // difference, so an incomplete walk records NO verdict.
    if !census.complete {
        log::warn!(
            "fsck C10: the inode walk did not complete, so a named inode's record may \
             be missing from the live set — the zero-count and dangling-name arms \
             record no verdict for this run (C1 owns the unreadable node)"
        );
        return;
    }
    let ceiling = (ino_set_byte_budget() / 8) as usize;
    let mut candidates: Vec<u64> = Vec::new();
    let mut deferred = 0u64;
    pass.refs.each_absent_from(&census.live, |ino| {
        if candidates.len() >= ceiling {
            deferred += 1;
            return;
        }
        candidates.push(ino);
    });
    if deferred > 0 {
        log::warn!(
            "fsck C10: {} named-but-not-live candidates examined this run, {deferred} \
             deferred to the next run (this volume's damage exceeds one batch — repair \
             what is reported and re-run; the class converges, like C9's)",
            candidates.len()
        );
    }
    candidates.sort_unstable();
    let mut dangling: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for ino in candidates {
        let (vol_idx, local) = ctx.meta.route_ino(ino);
        if !opts.owns_volume(vol_idx) {
            counters.inode_plane_foreign_scoped += 1;
            continue;
        }
        let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
            continue;
        };
        // PR 8: the lessee shard (see C9's gate).
        if !kv.inode_plane_owns_slot(local) {
            counters.inode_plane_foreign_slot_scoped += 1;
            continue;
        }
        // Symmetric PR 10 (review round 2, Issue 12): an ino a FOREIGN
        // appender's un-replayed ring window names is in flight at its
        // holder — its verdict is the next pass's, after the window is
        // checkpointed or recovered; every other ino is judged.
        if window_inos.contains(&ino) {
            counters.inode_plane_window_scoped += 1;
            continue;
        }
        match kv.read_inode_value_routed(local).await {
            // No record: the name resolves to nothing.
            Ok(None) => {
                dangling.insert(ino);
            }
            // A record whose count is 0 while names exist — unambiguous
            // damage (no legitimate state has it) and the shape
            // `destroy_inodes`' live-nlink skip no longer protects.
            Ok(Some(val)) if val.nlink == 0 => suspects.push(Suspect {
                kind: SuspectKind::C10ZeroNlinkNamed {
                    ino,
                    names: pass.record_names_of(ino),
                },
            }),
            // Live after all: the census visited this ino's key range
            // before the (concurrent) dentry pass reached its name, i.e.
            // the create window. Cleared, and counted as the guard working.
            Ok(Some(_)) => counters.nlink_transient_cleared += 1,
            Err(_) => {}
        }
    }
    if dangling.is_empty() {
        return;
    }
    // One extra dentry walk, paid only when a name really does resolve to
    // nothing: the dangling arm's object is the dentry RECORD (a collision
    // chain holds several keys for one name), so repair needs its identity.
    let (identities, _, _) = build_referenced_inos(
        ctx.meta.clone(),
        opts.shard,
        opts.throttle_pct,
        opts.cancel.clone(),
        Some(&dangling),
    )
    .await;
    let Some(identities) = identities else {
        log::warn!(
            "fsck C10: the dangling-name identity pass could not complete — those \
             candidates record no verdict for this run"
        );
        return;
    };
    for ino in {
        let mut v: Vec<u64> = dangling.into_iter().collect();
        v.sort_unstable();
        v
    } {
        for name in identities.collected(ino) {
            // The one inode-plane verdict that is UNDECIDABLE online
            // under multi-owner: the object of this arm is the dentry
            // RECORD, which lives on the parent's volume — and when that
            // volume is a peer's, this node can neither commit its
            // removal nor read it as anything but a projection, while the
            // peer cannot read this ino's record as anything but one.
            // Declined and counted, never guessed at; the offline
            // whole-set pass is its detector (§5.8.2's clause 3 strips
            // exactly this shape at the coordinator, so reporting it here
            // would also make `fsck_inode_plane_proposals_stripped` — a
            // must-stay-0 tripwire — grow on a healthy fleet).
            if !opts.owns_volume(name.vol) {
                counters.inode_plane_cross_owner_declined += 1;
                continue;
            }
            suspects.push(Suspect {
                kind: SuspectKind::C10Dangling {
                    vol: name.vol,
                    key: name.key.clone(),
                    parent: name.parent,
                    name: String::from_utf8_lossy(&name.name).to_string(),
                    child_ino: ino,
                    file_type: name.file_type,
                },
            });
        }
    }
}

/// One ino's CURRENT block mappings through the census extraction path
/// (inline or indirect map, the blob block included) — C9's evidence and
/// repair input.
async fn layout_mappings_of(ctx: &FsckCtx, ino: u64) -> Vec<MappingRef> {
    let (vol_idx, local) = ctx.meta.route_ino(ino);
    let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
        return Vec::new();
    };
    let Ok(Some(bytes)) = kv.getxattr(local, "layout").await else {
        return Vec::new();
    };
    let layout: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{") {
        serde_json::from_slice(&bytes).ok()
    } else {
        bincode::deserialize(&bytes).ok()
    };
    let Some(layout) = layout else {
        return Vec::new();
    };
    let block_size = ctx
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let mut probe = probe_census();
    let tree_entries = kvmap_entries_for(ctx, kv, local, &layout).await;
    census_layout(ctx, ino, &layout, block_size, tree_entries, &mut probe).await;
    probe.mappings
}

// ---------------------------------------------------------------------------
// C2/C3/C6 evaluation
// ---------------------------------------------------------------------------

/// C8 (pre-RC spec §6.2 item 1): durable-vs-derived block-reference
/// drift. Factored out of [`run`] so the KD-MW-16 fleet FINALIZE runs
/// the identical class (a fleet pass must never cover LESS than the
/// coordinator's own unsharded run). Skipped (with a warning, never a
/// guess) when the comparison fails; a volume without the ledger has
/// nothing to disagree with.
async fn evaluate_c8(ctx: &FsckCtx, suspects: &mut Vec<Suspect>) {
    if !ctx.meta.volumes.iter().any(|kv| kv.block_refs_engaged()) {
        return;
    }
    let chunk = ctx.router.backend_router.default_allocator.chunk_size();
    match ctx
        .router
        .backend_router
        .verify_durable_block_refs(&ctx.meta)
        .await
    {
        Ok(drift) => {
            for (vol, idx, durable, derived) in drift {
                suspects.push(Suspect {
                    kind: SuspectKind::C8DurableRefDrift {
                        vol,
                        offset: idx.saturating_mul(chunk),
                        durable,
                        derived,
                    },
                });
            }
        }
        Err(e) => log::warn!(
            "fsck C8: durable block-reference comparison failed: {e} (no verdict \
             recorded — the class is skipped for this run, never guessed)"
        ),
    }
}

/// C13 nomination (design-symmetric-metadata §5.8.5): per meta volume,
/// the grant-claimed image extents no tree root reaches — the backend's
/// census under its SMO + mint serialization, so the "live in-window
/// image" (an SMO's successor before its route flip, a lazy mint's root
/// before the forest names it) is structurally excluded and a retired
/// image parked on its region's tail is a pending-free, not a claim.
/// Empty on a flat volume and an unpartitioned forest (no grant exists).
/// Its own pass, not the census walk: the class needs the tree's NODE
/// reachability (every interior node's live child pointers), which the
/// record-level census does not read — one paged walk of the interior
/// population per volume, skipped when no grant holds a claim.
async fn evaluate_c13(ctx: &FsckCtx, suspects: &mut Vec<Suspect>) {
    for (vol, kv) in ctx.meta.volumes.iter().enumerate() {
        match kv.c13_orphan_image_extents().await {
            Ok(orphans) => {
                for o in orphans {
                    suspects.push(Suspect {
                        kind: SuspectKind::C13OrphanImageExtent {
                            vol,
                            appender: o.appender,
                            extent: o.extent,
                        },
                    });
                }
            }
            Err(e) => log::warn!(
                "fsck C13: orphan image-extent census failed on vol {vol}: {e} (no verdict \
                 recorded — the class is skipped for this volume, never guessed)"
            ),
        }
    }
}

/// C16 nomination (design-symmetric-metadata §5.8.5, PR 7): per data
/// volume, the shared-index drift census — the SHARED flags of every
/// mounted meta volume against the index home's entries. Empty on a set
/// without a forest home (the census returns nothing to compare).
async fn evaluate_c16(ctx: &FsckCtx, suspects: &mut Vec<Suspect>) {
    for (vol_tag, _alloc) in ctx.router.backend_router.durable_ref_volumes() {
        match crate::meta_backend::kv::shared_refs::shared_index_drift(&ctx.meta, vol_tag).await {
            Ok(drift) => {
                for d in drift {
                    let (r, flag_side) = match d {
                        crate::meta_backend::kv::shared_refs::SharedIndexDrift::FlagWithoutEntry(
                            r,
                        ) => (r, true),
                        crate::meta_backend::kv::shared_refs::SharedIndexDrift::EntryWithoutFlag(
                            r,
                        ) => (r, false),
                    };
                    suspects.push(Suspect {
                        kind: SuspectKind::C16SharedIndexDrift {
                            vol_tag,
                            block_idx: r.block_idx,
                            owner_ino: r.owner_ino,
                            block_index: r.block_index,
                            flag_side,
                        },
                    });
                }
            }
            Err(e) => log::warn!(
                "fsck C16: shared-index drift census failed for data volume {vol_tag:#x}: {e} \
                 (no verdict recorded — the class is skipped for this volume, never guessed)"
            ),
        }
    }
}

/// One C17 shape as nominated and confirmed (design-symmetric-metadata
/// §5.6.5): the four defects a striped directory can carry, each with the
/// objects the confirm pass re-reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum C17Shape {
    /// A stripe ino named by the maps of `dirs.len() ≥ 2` directories.
    MultiplyMapped { stripe: u64, dirs: Vec<u64> },
    /// A map's entry `index` names `stripe`, which has no inode record.
    MissingStripe { dir: u64, index: u16, stripe: u64 },
    /// A commit marker over fewer than two stripe entries, or a gap in the
    /// entry indices.
    IncompleteMap { dir: u64, entries: u16 },
    /// A dentry in stripe `index` whose `hash54 % K` is `routes_to`.
    MisroutedName {
        dir: u64,
        index: u16,
        stripe: u64,
        name: String,
        routes_to: u16,
    },
    /// A non-marker name in the directory's OWN tree after `migrating`
    /// cleared — a stale-route insert's trace (a create that routed the
    /// directory unstriped and inserted past the flag clear). Reads still
    /// serve it (the directory's own tree is the permanent secondary home)
    /// and its next mutation re-homes it; when its stripe holds the name
    /// too, the stripe's entry is the one served (the LWW rule).
    UnmigratedName {
        dir: u64,
        name: String,
        in_stripe_too: bool,
    },
}

impl C17Shape {
    fn dir(&self) -> u64 {
        match self {
            Self::MultiplyMapped { dirs, .. } => dirs.first().copied().unwrap_or(0),
            Self::MissingStripe { dir, .. }
            | Self::IncompleteMap { dir, .. }
            | Self::MisroutedName { dir, .. }
            | Self::UnmigratedName { dir, .. } => *dir,
        }
    }
    fn stripe(&self) -> u64 {
        match self {
            Self::MultiplyMapped { stripe, .. }
            | Self::MissingStripe { stripe, .. }
            | Self::MisroutedName { stripe, .. } => *stripe,
            Self::IncompleteMap { .. } | Self::UnmigratedName { .. } => 0,
        }
    }
    fn label(&self) -> &'static str {
        match self {
            Self::MultiplyMapped { .. } => "multiply-mapped",
            Self::MissingStripe { .. } => "missing-stripe",
            Self::IncompleteMap { .. } => "incomplete-map",
            Self::MisroutedName { .. } => "misrouted-name",
            Self::UnmigratedName { .. } => "unmigrated-name",
        }
    }
    fn name(&self) -> String {
        match self {
            Self::MisroutedName { name, .. } | Self::UnmigratedName { name, .. } => name.clone(),
            _ => String::new(),
        }
    }
    fn evidence(&self) -> String {
        match self {
            Self::MultiplyMapped { stripe, dirs } => format!(
                "stripe ino {stripe} is named by the stripe maps of {} directories ({dirs:?}) — \
                 a stripe belongs to exactly one directory; a create routed through either \
                 map lands in one tree both list",
                dirs.len()
            ),
            Self::MissingStripe { dir, index, stripe } => format!(
                "directory {dir}'s stripe map names ino {stripe} as stripe {index}, and no \
                 inode record exists for it — every name hashing to that stripe is \
                 unroutable"
            ),
            Self::IncompleteMap { dir, entries } => format!(
                "directory {dir} carries the stripe COMMIT marker over {entries} contiguous \
                 stripe entries (a live map has ≥ 2, indices 0..K) — the flip's intent \
                 should have written the whole map before the marker"
            ),
            Self::MisroutedName {
                dir,
                index,
                stripe,
                name,
                routes_to,
            } => format!(
                "name {name:?} sits in stripe {index} (ino {stripe}) of directory {dir} but \
                 hashes to stripe {routes_to} — a lookup routes by the hash and never finds it"
            ),
            Self::UnmigratedName {
                dir,
                name,
                in_stripe_too,
            } => format!(
                "name {name:?} is still in directory {dir}'s own tree after its migration \
                 flag cleared{} — a stale-route insert's trace: reads serve it from the \
                 directory's own tree{} and its next mutation re-homes it",
                if *in_stripe_too {
                    " (its stripe holds the name too)"
                } else {
                    ""
                },
                if *in_stripe_too {
                    " (the stripe's entry wins — the LWW rule)"
                } else {
                    ""
                }
            ),
        }
    }
}

/// C14 / C15 nomination (design-symmetric-metadata §5.8.5, PR 10): per
/// meta volume, the backend's slot custody census — every `Live` page's
/// `Live` slot entries against tree 0's `(g, lessee)` (C14) and every
/// `Live` / `Recovering` page whose identity volume 0's death ledger
/// names, with a ring window or leased slots (C15). Empty on a flat
/// volume and on an unpartitioned forest (no directory page but the
/// mount's own). The ledger is volume 0's tree 0; a set whose volume 0
/// is not mounted judges C14 alone.
async fn evaluate_c14_c15(ctx: &FsckCtx, suspects: &mut Vec<Suspect>) {
    // ONE volume-0 resolver across the driver and fsck (Issue 19).
    let vol0 = crate::meta_backend::kv::backend::recovery::vol0_of(&ctx.meta).map(|(_, v)| v);
    for (vol, kv) in ctx.meta.volumes.iter().enumerate() {
        match kv.slot_custody_census(vol0).await {
            Ok(census) => {
                for (slot, a, b) in census.conflicts {
                    suspects.push(Suspect {
                        kind: SuspectKind::C14SlotCustodyConflict {
                            vol,
                            slot,
                            appender_a: a,
                            appender_b: b,
                        },
                    });
                }
                for (appender, identity, window_entries) in census.unrecovered {
                    suspects.push(Suspect {
                        kind: SuspectKind::C15UnrecoveredAppender {
                            vol,
                            appender,
                            node_token: identity.node_token,
                            mount_slot: identity.mount_slot,
                            window_entries,
                        },
                    });
                }
            }
            Err(e) => log::warn!(
                "fsck C14/C15: slot custody census failed on vol {vol}: {e} (no verdict \
                 recorded — the classes are skipped for this volume, never guessed)"
            ),
        }
    }
}

/// One directory's stripe map as the marker census saw it.
#[derive(Default, Debug)]
struct C17Map {
    /// The commit marker's target (`Some` = the marker exists; a live map's
    /// names stripe 0 — `MARKER_TARGET_INDEX`).
    commit_target: Option<u64>,
    migrating: bool,
    entries: std::collections::BTreeMap<u16, u64>,
}

impl C17Map {
    fn striped(&self) -> bool {
        self.commit_target.is_some()
    }
}

/// Fold the marker census into per-directory maps (`global dir → map`).
fn c17_maps(
    meta: &RoutedMetaBackend,
    markers: &[(usize, u64, crate::meta_backend::dir_stripe::Marker, u64)],
) -> std::collections::BTreeMap<u64, C17Map> {
    use crate::meta_backend::dir_stripe::Marker;
    let mut maps: std::collections::BTreeMap<u64, C17Map> = std::collections::BTreeMap::new();
    for (vol, local_parent, marker, child) in markers {
        let Some(dir) = meta.try_make_global_ino(*local_parent, *vol) else {
            continue;
        };
        let m = maps.entry(dir).or_default();
        match marker {
            Marker::Striped => m.commit_target = Some(*child),
            Marker::Migrating => m.migrating = true,
            Marker::Stripe(i) => {
                m.entries.insert(*i, *child);
            }
        }
    }
    maps
}

/// The names of `dir`'s OWN dentry tree, markers excluded (paged).
async fn c17_own_names(meta: &RoutedMetaBackend, dir: u64) -> Vec<String> {
    let (v, local) = meta.route_ino(dir);
    let mut out = Vec::new();
    let mut cursor = 0u64;
    loop {
        let Ok(page) = meta.volumes[v].readdir_page(local, cursor, SCAN_PAGE).await else {
            break;
        };
        let Some((last, _)) = page.last() else { break };
        cursor = *last;
        out.extend(
            page.into_iter()
                .filter(|(_, e)| !crate::meta_backend::dir_stripe::is_marker_name(&e.name))
                .map(|(_, e)| e.name),
        );
    }
    out
}

/// Every C17 shape of ONE directory's map (the nomination's and the
/// confirm pass's shared derivation): the map's completeness, each
/// stripe's record and its names' routing, the directory's own leftover
/// names after the flag cleared.
async fn c17_shapes_of(meta: &RoutedMetaBackend, dir: u64, map: &C17Map) -> Vec<C17Shape> {
    use crate::meta_backend::dir_stripe::{stripe_of, MARKER_TARGET_INDEX};
    use crate::meta_backend::kv::record::dentry_name_hash54;
    let mut out = Vec::new();
    let Some(commit_target) = map.commit_target else {
        return out;
    };
    let k = map.entries.len();
    let contiguous = map
        .entries
        .keys()
        .enumerate()
        .all(|(i, idx)| usize::from(*idx) == i);
    // A live map's commit marker names stripe 0 (never the directory —
    // the reverse scan's self-hit, Issue 3); anything else is torn or
    // planted and the router ignores the map.
    let target_ok = map.entries.get(&MARKER_TARGET_INDEX) == Some(&commit_target);
    if k < 2 || !contiguous || !target_ok {
        out.push(C17Shape::IncompleteMap {
            dir,
            entries: k as u16,
        });
        return out;
    }
    let (dv, _) = meta.route_ino(dir);
    let seed = meta.volumes[dv].superblock().hash_seed;
    let k16 = k as u16;
    let mut stripe_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (index, stripe) in &map.entries {
        let (sv, slocal) = meta.route_ino(*stripe);
        let record = meta.volumes[sv].read_inode_value_routed(slocal).await;
        if !matches!(record, Ok(Some(_))) {
            out.push(C17Shape::MissingStripe {
                dir,
                index: *index,
                stripe: *stripe,
            });
            continue;
        }
        for name in c17_own_names(meta, *stripe).await {
            let routes_to = stripe_of(dentry_name_hash54(name.as_bytes(), seed), k16);
            if routes_to != *index {
                out.push(C17Shape::MisroutedName {
                    dir,
                    index: *index,
                    stripe: *stripe,
                    name: name.clone(),
                    routes_to,
                });
            }
            stripe_names.insert(name);
        }
    }
    if !map.migrating {
        for name in c17_own_names(meta, dir).await {
            out.push(C17Shape::UnmigratedName {
                dir,
                in_stripe_too: stripe_names.contains(&name),
                name,
            });
        }
    }
    out
}

/// C17 nomination (design-symmetric-metadata §5.6.5 / §5.8.5, PR 7b):
/// the marker census the ONE dentry walk carried, folded into maps; every
/// striped directory's stripes re-read (the striped population alone);
/// a stripe named by two maps found over the fold.
async fn evaluate_c17(
    ctx: &FsckCtx,
    markers: &[(usize, u64, crate::meta_backend::dir_stripe::Marker, u64)],
    suspects: &mut Vec<Suspect>,
) {
    let maps = c17_maps(&ctx.meta, markers);
    let mut named_by: std::collections::BTreeMap<u64, Vec<u64>> = std::collections::BTreeMap::new();
    for (dir, map) in &maps {
        if !map.striped() {
            continue;
        }
        for stripe in map.entries.values() {
            named_by.entry(*stripe).or_default().push(*dir);
        }
        for shape in c17_shapes_of(&ctx.meta, *dir, map).await {
            suspects.push(Suspect {
                kind: SuspectKind::C17StripeInconsistency(shape),
            });
        }
    }
    for (stripe, dirs) in named_by {
        if dirs.len() >= 2 {
            suspects.push(Suspect {
                kind: SuspectKind::C17StripeInconsistency(C17Shape::MultiplyMapped {
                    stripe,
                    dirs,
                }),
            });
        }
    }
}

/// C17's FRESH view for the whole confirm pass — ONE dentry walk (the
/// marker census) and each striped directory's shapes derived ONCE, every
/// C17 suspect judged against the same view (review round 1, Issue 14:
/// the per-suspect re-walk paid `N` whole-tree dentry walks for `N`
/// suspects — 64 on the crash shape that nominates every stripe of one
/// map). `None` = the census could not complete: no verdict (clears).
struct C17Fresh {
    maps: std::collections::BTreeMap<u64, C17Map>,
    shapes: HashMap<u64, Vec<C17Shape>>,
}

impl C17Fresh {
    async fn build(ctx: &FsckCtx) -> Option<Self> {
        let (Some(fresh), _, _) = build_referenced_inos(
            Arc::clone(&ctx.meta),
            None,
            0,
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .await
        else {
            return None;
        };
        Some(Self {
            maps: c17_maps(&ctx.meta, &fresh.markers),
            shapes: HashMap::new(),
        })
    }

    /// Does `shape` still hold on the fresh view?
    async fn holds(&mut self, ctx: &FsckCtx, shape: &C17Shape) -> bool {
        match shape {
            C17Shape::MultiplyMapped { stripe, .. } => {
                self.maps
                    .values()
                    .filter(|m| m.striped() && m.entries.values().any(|s| s == stripe))
                    .count()
                    >= 2
            }
            other => {
                let dir = other.dir();
                let Some(map) = self.maps.get(&dir) else {
                    return false;
                };
                let shapes = match self.shapes.get(&dir) {
                    Some(s) => s,
                    None => {
                        let derived = c17_shapes_of(&ctx.meta, dir, map).await;
                        self.shapes.entry(dir).or_insert(derived)
                    }
                };
                shapes.iter().any(|s| s == other)
            }
        }
    }
}

/// The GLOBAL inos every FOREIGN appender ring window this open did not
/// replay names, across the set — C9/C10's per-ino exclusion (each ring
/// read once). `None` = a ring could not be read: unknown = pending, no
/// verdict.
async fn foreign_window_inos(ctx: &FsckCtx) -> Option<std::collections::BTreeSet<u64>> {
    let mut out = std::collections::BTreeSet::new();
    let width = ctx.meta.routing_width();
    for (vol, kv) in ctx.meta.volumes.iter().enumerate() {
        match kv.foreign_window_inos(width).await {
            Ok(inos) => out.extend(inos),
            Err(e) => {
                log::warn!(
                    "fsck inode plane: reading vol {vol}'s foreign appender ring windows \
                     failed: {e} — the plane takes no verdict this run"
                );
                return None;
            }
        }
    }
    Some(out)
}

/// One owner's tree-7 record count, paged (`block_map_range`, the same
/// primitive the shared extraction rides) — C11's evidence unit. A
/// decode failure ends the count at what was read (conservative; C1's
/// walk owns unreadable records).
async fn kvmap_record_count(
    kv: &crate::meta_backend::kv::backend::KvMetaBackend,
    local_ino: u64,
) -> u64 {
    let mut n = 0u64;
    let mut cursor = 0u32;
    loop {
        let page = match kv.block_map_range(local_ino, cursor, SCAN_PAGE).await {
            Ok(p) => p,
            Err(_) => break,
        };
        let Some(last) = page.last().map(|(i, _)| *i) else {
            break;
        };
        n += page.len() as u64;
        let Some(next) = last.checked_add(1) else {
            break;
        };
        cursor = next;
    }
    n
}

/// `local_ino`'s durable layout head, decoded. `None` = no layout /
/// undecodable (the latter is C1's business).
async fn kvmap_head_of(
    kv: &crate::meta_backend::kv::backend::KvMetaBackend,
    local_ino: u64,
) -> Option<crate::routing::LayoutMetadata> {
    let bytes = kv.getxattr(local_ino, "layout").await.ok()??;
    if bytes.starts_with(b"{") {
        serde_json::from_slice(&bytes).ok()
    } else {
        bincode::deserialize(&bytes).ok()
    }
}

fn head_is_kvmap(layout: &Option<crate::routing::LayoutMetadata>) -> bool {
    layout
        .as_ref()
        .and_then(|l| l.block_map_id.as_deref())
        .is_some_and(|id| id.starts_with(crate::meta_backend::kv::block_map::KVMAP_HEAD_PREFIX))
}

/// C11 (a) nomination — orphan map records (design-kvmap-block-map-tree
/// §3 fsck + A3): tree-7 records whose owner ino has no live inode
/// record (a dead ino — the crashed unlink/crossing residue class) or
/// whose head is not kvmap-class (a crashed crossing's staged chunks,
/// invisible to reads, reclaimed only by the next crossing's A1 sweep).
///
/// The tree side is the owner SKIP-SCAN (one probe per distinct owner,
/// never per record — `block_map_owner_scan`); the reverse "does the
/// live head reach the tree" question is every other walker's arm, so
/// this pass is the ONLY detector of records no head names. The A3
/// registry shield applies at nomination AND at the verify arm; C9's
/// era floor is structurally inapplicable here (a record's ino may be
/// years old while its crossing is live on THIS mount).
async fn evaluate_c11_orphans(
    ctx: &FsckCtx,
    counters: &mut FsckCounters,
    suspects: &mut Vec<Suspect>,
) {
    for (vol_idx, kv) in ctx.meta.volumes.iter().enumerate() {
        if !kv.block_map_tree_engaged() {
            continue;
        }
        let owners = match kv.block_map_owner_scan().await {
            Ok(o) => o,
            Err(e) => {
                log::warn!(
                    "fsck C11: tree-7 owner scan failed on vol {vol_idx}: {e} — the map \
                     plane records no verdict for this volume (never guessed)"
                );
                continue;
            }
        };
        for local_ino in owners {
            // The A3 registry shield: an incomplete pass records no
            // verdict for a registered ino.
            if kv.crossing_in_flight(local_ino) {
                counters.crossing_exempted += 1;
                continue;
            }
            let live = match kv.read_inode_value_routed(local_ino).await {
                Ok(v) => v.is_some(),
                Err(_) => continue, // unreadable: C1's business, no verdict
            };
            if live && head_is_kvmap(&kvmap_head_of(kv, local_ino).await) {
                continue; // healthy: the head names the tree
            }
            suspects.push(Suspect {
                kind: SuspectKind::C11OrphanMapRecords {
                    vol: vol_idx,
                    local_ino,
                },
            });
        }
    }
}

/// One owner's C11 (c) evidence scan (PR 6a, design §12): point records
/// on a DIFFERENT volume strictly inside a covering run's span, as
/// `(run_start, idx)` pairs. Same-volume points inside a run are LEGAL
/// by the §2 read law (the arithmetic-equal shadow and the overwrite
/// class alike — and without the router's stride census this walker
/// cannot tell them apart, which is fine: the report arm needs only the
/// volume tags). STRING records carry no tag and record no verdict.
/// Record order is key order, so a covering run always precedes the
/// points inside its span.
async fn kvmap_run_foreign_shadows(
    kv: &crate::meta_backend::kv::backend::KvMetaBackend,
    local_ino: u64,
) -> Vec<(u32, u32)> {
    use crate::meta_backend::kv::block_map::MapEntry;
    let mut out = Vec::new();
    // The open covering run: (start, end_exclusive, vol_tag).
    let mut open_run: Option<(u32, u64, u64)> = None;
    let mut cursor = 0u32;
    loop {
        let page = match kv.block_map_range(local_ino, cursor, SCAN_PAGE).await {
            Ok(p) => p,
            Err(_) => break, // unreadable: C1's business, no verdict
        };
        let Some(last) = page.last().map(|(i, _)| *i) else {
            break;
        };
        for (idx, entry) in page {
            match entry {
                MapEntry::Run { vol_tag, len, .. } | MapEntry::RunStamped { vol_tag, len, .. } => {
                    open_run = Some((idx, u64::from(idx) + u64::from(len), vol_tag));
                }
                MapEntry::Point { vol_tag, .. } | MapEntry::PointStamped { vol_tag, .. } => {
                    if let Some((start, end, run_tag)) = open_run {
                        if u64::from(idx) > u64::from(start)
                            && u64::from(idx) < end
                            && vol_tag != run_tag
                        {
                            out.push((start, idx));
                        }
                    }
                }
                MapEntry::String(_) => {}
            }
        }
        let Some(next) = last.checked_add(1) else {
            break;
        };
        cursor = next;
    }
    out
}

/// C11 (c) nomination — the PR 6a run-vs-point coverage sanity arm
/// (design §12): rides the same owner skip-scan as (a), with the same A3
/// registry shield.
async fn evaluate_c11_run_coverage(
    ctx: &FsckCtx,
    counters: &mut FsckCounters,
    suspects: &mut Vec<Suspect>,
) {
    for (vol_idx, kv) in ctx.meta.volumes.iter().enumerate() {
        if !kv.block_map_tree_engaged() {
            continue;
        }
        let owners = match kv.block_map_owner_scan().await {
            Ok(o) => o,
            Err(e) => {
                log::warn!(
                    "fsck C11 (c): tree-7 owner scan failed on vol {vol_idx}: {e} — the map \
                     plane records no verdict for this volume (never guessed)"
                );
                continue;
            }
        };
        for local_ino in owners {
            if kv.crossing_in_flight(local_ino) {
                counters.crossing_exempted += 1;
                continue;
            }
            for (run_start, idx) in kvmap_run_foreign_shadows(kv, local_ino).await {
                suspects.push(Suspect {
                    kind: SuspectKind::C11RunForeignShadow {
                        vol: vol_idx,
                        local_ino,
                        run_start,
                        idx,
                    },
                });
            }
        }
    }
}

/// C11 (b) nomination — head/tree coverage mismatch, the FULLY-EMPTY
/// case only (a `kvmap:1` head with nonzero size and ZERO tree records;
/// the size-vs-sparse ambiguity makes any partial-coverage verdict
/// unsafe, so it is deliberately out of scope). Candidates ride the
/// census walk (which already pages the kvmap extraction per layout);
/// the verify ladder owns every verdict.
fn evaluate_c11_empty_heads(
    census: &CensusOut,
    counters: &mut FsckCounters,
    suspects: &mut Vec<Suspect>,
    meta: &RoutedMetaBackend,
) {
    for &(vol, local_ino, size) in &census.kvmap_empty_heads {
        // The A3 registry shield at nomination (re-checked at verify).
        if meta
            .volumes
            .get(vol)
            .is_some_and(|kv| kv.crossing_in_flight(local_ino))
        {
            counters.crossing_exempted += 1;
            continue;
        }
        suspects.push(Suspect {
            kind: SuspectKind::C11EmptyKvmapHead {
                vol,
                local_ino,
                size,
            },
        });
    }
}

/// C12 — tenant-range consistency (design-small-file-packing §5.9). Rides
/// the census mapping list — NO new walk: one interval sort per
/// `(vol, offset, incarnation)` with same-`off` windows collapsed to their
/// longest, then a sweep for a window that starts inside the furthest
/// reach so far at a DIFFERENT `off`. Same-`off` windows of any lengths
/// are the two legal share classes (the clone's identical window; the
/// clone + passthrough clip's nested prefix) and never nominate; two
/// whole-block referencers are both `[0, chunk)` and collapse. A key on a
/// bit-13 volume names a LIFETIME, so two mappings naming different
/// incarnations of one offset are not one block and never meet here.
/// Every decorated mapping is also judged against the window law
/// (C12Overrun — including the undecodable decoration the census resolves
/// through its base and would otherwise never report). Quarantined
/// (`damaged:`) mappings are the repair, not a window.
fn evaluate_c12_tenant_ranges(census: &CensusOut, suspects: &mut Vec<Suspect>) {
    let chunk = crate::block_allocator::CHUNK_SIZE;
    let mut groups: HashMap<(&str, u64, u64), Vec<TenantWindow>> = HashMap::new();
    for m in census.mappings.iter().filter(|m| !m.damaged) {
        match tenant_window_class(&m.mapping, chunk) {
            WindowClass::Overrun(why) => suspects.push(Suspect {
                kind: SuspectKind::C12Overrun {
                    vol: m.vol.clone(),
                    offset: m.offset,
                    ino: m.ino,
                    block_idx: m.block_idx,
                    mapping: m.mapping.clone(),
                    why,
                },
            }),
            WindowClass::Window { off, end } => {
                let inc = crate::routing::block_key_incarnation(&m.mapping)
                    .unwrap_or(crate::routing::INCARNATION_NONE);
                groups
                    .entry((m.vol.as_str(), m.offset, inc))
                    .or_default()
                    .push(TenantWindow {
                        ino: m.ino,
                        block_idx: m.block_idx,
                        mapping: m.mapping.clone(),
                        off,
                        end,
                    });
            }
        }
    }
    for ((vol, offset, _inc), mut windows) in groups {
        if windows.len() < 2 {
            continue;
        }
        // Ascending `off`, longest first within an `off`: the first window
        // of each `off` IS the group's `max(len)` collapse.
        windows.sort_by(|a, b| a.off.cmp(&b.off).then(b.end.cmp(&a.end)));
        let mut reach: Option<&TenantWindow> = None;
        let mut current_off: Option<u64> = None;
        for w in &windows {
            if current_off == Some(w.off) {
                continue;
            }
            current_off = Some(w.off);
            match reach {
                None => reach = Some(w),
                Some(r) => {
                    if w.intersects_at_different_off(r) {
                        suspects.push(Suspect {
                            kind: SuspectKind::C12Overlap {
                                vol: vol.to_string(),
                                offset,
                                a: r.clone(),
                                b: w.clone(),
                            },
                        });
                    }
                    if w.end > r.end {
                        reach = Some(w);
                    }
                }
            }
        }
    }
}

fn evaluate_allocator_classes(
    vols: &[VolAlloc],
    census: &CensusOut,
    opts: &FsckOptions,
    counters: &mut FsckCounters,
    suspects: &mut Vec<Suspect>,
) {
    static EMPTY: once_cell::sync::Lazy<HashMap<u64, u32>> =
        once_cell::sync::Lazy::new(HashMap::new);
    let sharded = opts.shard.is_some();
    for v in vols {
        let refs = census.refs.get(&v.id).unwrap_or(&EMPTY);
        let tracked: HashMap<u64, u32> = v.alloc.tracked_offsets().into_iter().collect();
        let capacity = v.alloc.capacity_bytes();
        let chunk = v.alloc.chunk_size();
        // DLM S9 (rung-10 finding #5): under an ENGAGED allocation
        // partition this mount's allocator census covers only the lanes it
        // OWNS. A live peer writer's blocks are durably referenced and
        // structurally untracked HERE — untracked across every scan epoch,
        // which is exactly what the settle ladder cannot clear — so a
        // foreign-lane offset is a PEER's to adjudicate, never a lost
        // finding on this mount (exempt, counted). The cross-writer
        // oracle stays C8 (the durable ledger census); the lane-aligned
        // fleet-parallel fsck is rung 10c's (KD-MW-16).
        let lane_view = v
            .alloc
            .lane_partition()
            .map(|p| (p.writers(), v.alloc.owned_lane_mask().unwrap_or(1)));
        let foreign_lane = |off: u64| match lane_view {
            None => false,
            Some((w, owned)) => {
                owned & (1u64 << crate::data_alloc_lane::offset_lane_of(off, chunk, w)) == 0
            }
        };
        // Symmetric PR 13 (defect 27): on a grant-armed allocator whose
        // allocation lease THIS process holds, the RAM refcount map knows
        // this mount's own mints and its mount-time census alone — a block
        // a FORMER lessee minted from its grant window (a slot released,
        // handed over or recovered to this mount since) is durably
        // referenced and SET in the holder's bitmap, never in RAM. The
        // bitmap is the allocation truth there (PR 8 — the free list IS
        // the bitmap): a referenced offset whose bit is SET is TRACKED, not
        // lost. Counted apart from the lane exemption.
        let bitmap_set: Option<Arc<crate::meta_backend::kv::alloc_lease::AllocHolding>> = v
            .alloc
            .block_grant_vol_tag()
            .and_then(crate::meta_backend::kv::alloc_lease::holding);
        let bitmap_tracked = |off: u64| {
            bitmap_set
                .as_ref()
                .is_some_and(|h| h.bitmap.is_set(off / chunk))
        };

        // Leaked / C3: allocator-side ground truth is mount-session RAM —
        // meaningless on a sharded walk (a shard sees only its residue's
        // references) and on offline probes without a recovery walk.
        if !sharded {
            for (&off, &rc) in &tracked {
                counters.refcounts_checked += 1;
                match refs.get(&off) {
                    None => suspects.push(Suspect {
                        kind: SuspectKind::C2Leaked {
                            vol: v.id.clone(),
                            offset: off,
                        },
                    }),
                    Some(&n) if n != rc => suspects.push(Suspect {
                        kind: SuspectKind::C3Refcount {
                            vol: v.id.clone(),
                            offset: off,
                            expected: n,
                            actual: rc,
                        },
                    }),
                    Some(_) => {}
                }
            }
        }

        // Lost: referenced but untracked, out-of-range, or unaligned.
        // Referencers that are §5.6a `damaged:` quarantine markers are
        // never lost findings — the mapping IS the repair (the marker
        // preserves the reference for forensics and reads EIO).
        for (&off, _) in refs.iter() {
            let referencers: Vec<&MappingRef> = census
                .mappings
                .iter()
                .filter(|m| m.vol == v.id && m.offset == off)
                .collect();
            let Some(live) = referencers.iter().find(|m| !m.damaged) else {
                // No referencer at all (cross-volume alias) or every
                // referencer already quarantined: nothing to report.
                continue;
            };
            let lost = |why: String| Suspect {
                kind: SuspectKind::C2Lost {
                    vol: v.id.clone(),
                    offset: off,
                    ino: live.ino,
                    block_idx: live.block_idx,
                    mapping: live.mapping.clone(),
                    why,
                },
            };
            if capacity != 0 && off >= capacity {
                suspects.push(lost(format!("offset past device capacity {capacity}")));
            } else if off % chunk != 0 {
                suspects.push(lost(format!(
                    "offset not aligned to the {chunk} B allocator chunk"
                )));
            } else if !sharded && !tracked.contains_key(&off) {
                if foreign_lane(off) {
                    counters.foreign_lane_exempted += 1;
                } else if bitmap_tracked(off) {
                    counters.alloc_bitmap_tracked_exempted += 1;
                } else {
                    suspects.push(lost(
                        "referenced offset is not allocator-tracked".to_string(),
                    ));
                }
            }
        }

        // C6: used-blocks arithmetic vs the tracked population. In-flight-
        // registered offsets are exempt from the arithmetic (the same §5.6
        // live-owner shield C2/C3 consult per offset): an offset in the
        // allocate→publish window or the begin_free→reclaim window is
        // "used" by the arithmetic but deliberately untracked — and since
        // the write-wall manners law the begin_free limbo legitimately
        // spans whole foreground-busy periods (the deferred reclaim
        // backlog), so the old settle-window absorption can no longer
        // cover it (the 2026-07-31 iteration-loop FP).
        if !sharded && lane_view.is_some() {
            // Rung-10 finding #5, the C6 half: the used-vs-tracked
            // arithmetic is SINGLE-WRITER by construction — `highest −
            // free − inflight − graced` spans every lane while `tracked`
            // spans only this mount's, so on any engaged partition the
            // census reads drift the size of the peers' whole population.
            // C6 therefore DECLINES here (the doctrine that already
            // declines foreign-lane reconciliation), counted on the same
            // exemption gauge; C8 remains the multi-writer capacity oracle.
            counters.foreign_lane_exempted += 1;
            log::info!(
                "fsck C6: capacity census declined on '{}' — a {}-way allocation partition is \
                 engaged and the used-vs-tracked arithmetic is single-writer by construction \
                 (fsck_foreign_lane_exempted; C8 is the multi-writer oracle)",
                v.id,
                lane_view.map(|(w, _)| w).unwrap_or(1),
            );
        } else if !sharded && v.alloc.block_grant_armed() {
            // Symmetric PR 8/10: on a grant-armed allocator the bitmap IS
            // the free list — the local list was drained into it at the
            // arm and a freed block returns only through a carve — so
            // `highest − free_list` is single-writer arithmetic that reads
            // every bitmap-clear block below the cursor as used (the
            // `sym-crash` leg read one crash's released window as a
            // 386-vs-385 finding). C6's arm here is PR 8's BITMAP ORACLE
            // (`DataAllocBitmap::drift`, review round 1, Issue 11 — the
            // first build declined and lost the fsck-time census): the
            // bitmap against this census's tracked population, with the
            // holder's OPEN grant ranges and the in-flight registry as the
            // live-owner shield. LOSS (referenced ∧ clear — a future carve
            // would overwrite a live block) is the report-only finding;
            // LEAK (set ∧ ¬referenced ∧ ¬granted ∧ ¬in-flight) is a dead
            // incarnation's remainder the next (re-)hold releases, counted.
            let chunk = v.alloc.chunk_size().max(1);
            let holding = v
                .alloc
                .block_grant_vol_tag()
                .and_then(crate::meta_backend::kv::alloc_lease::holding);
            match holding {
                Some(h) => {
                    let referenced: std::collections::BTreeSet<u64> =
                        tracked.keys().map(|off| off / chunk).collect();
                    let report = h.bitmap.drift(&referenced, &h.ledger.open_ranges());
                    let inflight: std::collections::BTreeSet<u64> = v
                        .alloc
                        .inflight_offsets()
                        .into_iter()
                        .map(|off| off / chunk)
                        .collect();
                    let leaks = report.leak.iter().filter(|b| !inflight.contains(b)).count() as u64;
                    counters.alloc_bitmap_leak_candidates += leaks;
                    if !report.loss.is_empty() {
                        suspects.push(Suspect {
                            kind: SuspectKind::C6Drift {
                                vol: v.id.clone(),
                                used: h.bitmap.population(),
                                tracked: referenced.len() as u64,
                            },
                        });
                        log::warn!(
                            "fsck C6 (bitmap oracle): data volume '{}' — {} referenced block(s) \
                             read CLEAR in the allocation bitmap (first {:?}); the LOSS class",
                            v.id,
                            report.loss.len(),
                            report.loss.first()
                        );
                    }
                }
                None => {
                    // A grant-armed allocator whose holding this process
                    // does not keep (a wire writer's — PR 12's venue): no
                    // bitmap to judge against; declined, counted.
                    counters.foreign_lane_exempted += 1;
                    log::info!(
                        "fsck C6: capacity census declined on '{}' — the allocator mints from \
                         ranged block grants and this process holds no allocation lease for it \
                         (fsck_foreign_lane_exempted; the holder's bitmap oracle is the census)",
                        v.id
                    );
                }
            }
        } else if !sharded {
            let inflight = v
                .alloc
                .inflight_offsets()
                .into_iter()
                .filter(|off| !tracked.contains_key(off))
                .count() as u64;
            // Spec §6.8 item 3: offsets held in the freed-offset grace
            // period are untracked AND deliberately not free-listed — their
            // free completed, only the free-list publish waits on the
            // readers' acknowledgements. They are "used" to the arithmetic
            // and tracked by nothing, exactly like the begin_free→reclaim
            // limbo the in-flight term above exempts, so they get the same
            // exemption. Without it every reader-armed mount reports C6
            // drift for as long as it is churning, and `fsck_findings` must
            // stay 0 on a healthy volume.
            let graced = v.alloc.grace_len() as u64;
            let used = v
                .alloc
                .highest_block_index()
                .saturating_sub(v.alloc.free_blocks_count())
                .saturating_sub(inflight)
                .saturating_sub(graced);
            let tracked_count = tracked.len() as u64;
            if used != tracked_count {
                suspects.push(Suspect {
                    kind: SuspectKind::C6Drift {
                        vol: v.id.clone(),
                        used,
                        tracked: tracked_count,
                    },
                });
            }
        }
    }

    // Unresolvable mappings are lost by definition (unknown backend id /
    // unparseable key — the retired-id straggler class, R8).
    for m in &census.unresolvable {
        suspects.push(Suspect {
            kind: SuspectKind::C2Lost {
                vol: "?".to_string(),
                offset: 0,
                ino: m.ino,
                block_idx: m.block_idx,
                mapping: m.mapping.clone(),
                why: format!("mapping '{}' resolves to no known backend", m.mapping),
            },
        });
    }
}

// ---------------------------------------------------------------------------
// C4/C5 staging scan
// ---------------------------------------------------------------------------

/// Parse `active_block[_ext]:inode_{ino}:block_{b}` custody keys.
fn custody_key_ino(key: &str) -> Option<u64> {
    let rest = key
        .strip_prefix("active_block_ext:")
        .or_else(|| key.strip_prefix("active_block:"))?;
    let rest = rest.strip_prefix("inode_")?;
    let ino_str = rest.split(':').next()?;
    ino_str.parse::<u64>().ok()
}

/// The C4/C5 staging scan — spawnable (VL10, G-VL-5(c)): it touches only
/// the staging dirs and per-key `getattr`, independent of the C1 walks
/// and the census, so the unthrottled pass 1 overlaps it with them.
async fn scan_staging(
    meta: Arc<RoutedMetaBackend>,
    staging_dirs: Vec<PathBuf>,
    expected_generation: Option<String>,
    shard: Option<(u32, u32)>,
) -> Vec<Suspect> {
    let mut suspects = Vec::new();
    for dir in &staging_dirs {
        // C5: generation validity.
        if let Some(expected) = &expected_generation {
            match crate::cache::nvme::read_staging_generation_marker(dir).await {
                // §6.2 item 10: a root still bound to the UN-scoped
                // generation of a now-scoped set is the pre-upgrade shape
                // the next mount adopts, not a finding
                // (`marker_is_rebindable` = Match | ScopeUpgrade).
                Ok(Some(found)) if crate::writer_scope::marker_is_rebindable(&found, expected) => {}
                Ok(Some(found)) => suspects.push(Suspect {
                    kind: SuspectKind::C5Generation {
                        dir: dir.clone(),
                        why: format!(
                            "generation marker '{found}' does not match the mounted \
                             volume-set generation '{expected}'"
                        ),
                    },
                }),
                Ok(None) => {
                    if crate::cache::nvme::dir_has_segment_data(&dir.join("staging_segment")) {
                        suspects.push(Suspect {
                            kind: SuspectKind::C5Generation {
                                dir: dir.clone(),
                                why: "staging dir holds segment data with no readable \
                                      generation marker"
                                    .to_string(),
                            },
                        });
                    }
                }
                Err(e) => suspects.push(Suspect {
                    kind: SuspectKind::C5Generation {
                        dir: dir.clone(),
                        why: format!("generation marker unreadable: {e}"),
                    },
                }),
            }
        }

        // C4: orphan custody records.
        let keys = match crate::cache::nvme::scan_live_staged_custody(dir, CUSTODY_SCAN_MAX).await {
            Ok(keys) => keys,
            Err(e) => {
                log::warn!("fsck: staged custody scan of {} failed: {e}", dir.display());
                continue;
            }
        };
        for key in keys {
            let Some(ino) = custody_key_ino(&key) else {
                continue;
            };
            if let Some((k, n)) = shard {
                if ino % n as u64 != k as u64 {
                    continue;
                }
            }
            let missing = match meta.getattr(ino).await {
                Ok(inode) => inode.nlink == 0,
                Err(e) if is_not_found(&e) => true,
                Err(_) => false, // infrastructure error: never a finding
            };
            if missing {
                suspects.push(Suspect {
                    kind: SuspectKind::C4Orphan {
                        dir: dir.clone(),
                        key,
                        ino,
                    },
                });
            }
        }
    }
    suspects
}

// ---------------------------------------------------------------------------
// Re-check (settle done): the class-specific verification ladders
// ---------------------------------------------------------------------------

async fn recheck_suspects(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    vols: &[VolAlloc],
    suspects: Vec<Suspect>,
    counters: &mut FsckCounters,
    findings: &mut Vec<FsckFinding>,
) -> Result<()> {
    let online = opts.mode == FsckMode::Online;
    let ledger: Vec<String> = crate::jobs::mover_prepublish_ledger()
        .into_iter()
        .map(|k| clean_key(&k))
        .collect();
    // The small-file packer's OPEN pack blocks (design-small-file-packing
    // §5.3): each holds the packer's +1 PIN — a RAM reference no layout
    // justifies while the block is open — so an open pack reads C3 (RAM
    // refcount = tenants + 1 vs the census's tenants) or, before its first
    // tenant committed, C2Leaked (refcount 1, no referencer). The ledger
    // excuses exactly that pin; the tenants' transient references are
    // the registry's (checked first).
    // Resolved to `(allocator, offset)` through the router — the ledger's
    // key spelling is the PACKER's (`persist_block_key` of the placement
    // pick's backend id), which need not match the census's volume id on
    // a bare router; allocator identity + offset is the one comparison
    // every spelling reduces to.
    let pack_ledger: Vec<(Arc<BlockAllocator>, u64)> = crate::jobs::pack_open_ledger()
        .iter()
        .filter_map(|k| ctx.router.backend_router.allocator_for_key(k))
        .collect();
    let pack_open = |alloc: &Arc<BlockAllocator>, offset: u64| {
        pack_ledger
            .iter()
            .any(|(a, o)| Arc::ptr_eq(a, alloc) && *o == offset)
    };
    let alloc_of = |vol: &str| vols.iter().find(|v| v.id == vol).map(|v| v.alloc.clone());

    // Phase A (C2/C3): epoch filter, then — for two-epoch survivors —
    // the in-flight registry, THEN the mover ledger, THEN the pack-open
    // ledger. Registry-absence strictly precedes the Phase-B reference
    // re-read (the §5.6 normative order; the hook marks the boundary).
    let mut pending: Vec<Suspect> = Vec::new();
    for s in suspects {
        match &s.kind {
            SuspectKind::C2Leaked { vol, offset }
            | SuspectKind::C2Lost { vol, offset, .. }
            | SuspectKind::C3Refcount { vol, offset, .. } => {
                let Some(alloc) = alloc_of(vol) else {
                    pending.push(s);
                    continue;
                };
                if online && alloc.allocation_epoch_of(*offset).is_some() {
                    counters.epoch_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                let key = ctx.router.backend_router.persist_block_key(vol, *offset);
                if online {
                    fire_pre_registry_hook(&key);
                }
                if online && alloc.inflight_contains(*offset) {
                    counters.inflight_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                if ledger.iter().any(|k| k == &key || k == &clean_key(&key)) {
                    counters.mover_ledger_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                if pack_open(&alloc, *offset) {
                    counters.pack_ledger_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                pending.push(s);
            }
            // C12: an OPEN pack block is exempt — its slots are being
            // minted right now (design-small-file-packing §5.9): the
            // tenants mid-flight ride the in-flight registry, the pack
            // itself the pack-open ledger. Same hook boundary as C2/C3 so
            // a test can race the census against the verify.
            SuspectKind::C12Overlap { vol, offset, .. }
            | SuspectKind::C12Overrun { vol, offset, .. } => {
                let Some(alloc) = alloc_of(vol) else {
                    pending.push(s);
                    continue;
                };
                let key = ctx.router.backend_router.persist_block_key(vol, *offset);
                if online {
                    fire_pre_registry_hook(&key);
                }
                if online && alloc.inflight_contains(*offset) {
                    counters.inflight_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                if pack_open(&alloc, *offset) {
                    counters.pack_ledger_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                pending.push(s);
            }
            _ => pending.push(s),
        }
    }

    // Phase B: ONE fresh reference walk (after every registry check —
    // an owner that deregistered before its Phase-A read has, by the
    // registry contract, already made its publish visible to this walk).
    let needs_fresh = pending.iter().any(|s| {
        matches!(
            s.kind,
            SuspectKind::C2Leaked { .. }
                | SuspectKind::C2Lost { .. }
                | SuspectKind::C3Refcount { .. }
                | SuspectKind::C6Drift { .. }
                | SuspectKind::C8DurableRefDrift { .. }
        )
    });
    let fresh = if needs_fresh {
        Some(walk_census(ctx, opts, counters).await?)
    } else {
        None
    };
    // Phase B for the inode plane: ONE fresh referenced-ino pass after the
    // settle window, carrying C10's name identities for the inos it is
    // judging.
    //
    // For C9 this is the arm that catches the only live way a name can
    // appear for an inode nothing could reach — an `open_by_handle_at`
    // reconnect, or an S9 plan's late `InsertDentry` — and clears the
    // suspect. Offline needs it not (nothing is in flight by definition),
    // and an INCOMPLETE fresh pass clears every C9 suspect rather than
    // confirming one from a partial set.
    //
    // For C10 the same pass supplies every verdict's numerator: the
    // DEDUPED `(global parent, name)` identities of the nominated inos.
    // C10 needs it in BOTH modes (the nomination counted records, which
    // over-count during a slot migration), so the pass runs whenever
    // either class has work — and the C9 arm keeps its own online-only
    // gate below, unchanged.
    let c10_judged: std::collections::HashSet<u64> = pending
        .iter()
        .filter_map(|s| match &s.kind {
            SuspectKind::C10NlinkMismatch { ino, .. }
            | SuspectKind::C10ZeroNlinkNamed { ino, .. } => Some(*ino),
            _ => None,
        })
        .collect();
    let c9_pending = pending
        .iter()
        .any(|s| matches!(s.kind, SuspectKind::C9Unreferenced { .. }));
    // The C10 record witness, half one: `(nlink, ctime)` under the ino's
    // exclusive 4a lease, read BEFORE the fresh pass. Every same-volume op
    // that moves an inode's name count mutates its record in the same
    // transaction, so a witness that survives the whole pass is what makes
    // the pass's read-over-time skew (two dentry pages either side of one
    // atomic rename) unable to produce a finding. Acquired and released one
    // ino at a time: fsck never holds a lease set across a walk.
    let mut c10_witness: HashMap<u64, (u32, u64)> = HashMap::new();
    if online {
        let mut inos: Vec<u64> = c10_judged.iter().copied().collect();
        inos.sort_unstable();
        for ino in inos {
            let (vol_idx, local) = ctx.meta.route_ino(ino);
            let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
                continue;
            };
            let _lease = kv.dlm().lock_inode_exclusive(local).await;
            if let Ok(Some(v)) = kv.read_inode_value_routed(local).await {
                c10_witness.insert(ino, (v.nlink, v.ctime));
            }
        }
    }
    let fresh_pass = if (online && c9_pending) || !c10_judged.is_empty() {
        let (refs, indexed, _) = build_referenced_inos(
            ctx.meta.clone(),
            opts.shard,
            opts.throttle_pct,
            opts.cancel.clone(),
            Some(&c10_judged),
        )
        .await;
        counters.dentry_refs_indexed += indexed;
        Some(refs)
    } else {
        None
    };
    // C9's three-way gate, byte-for-byte as before: `Some(None)` = an
    // incomplete pass (clears), `None` = offline (nothing in flight).
    let fresh_refs = if online && c9_pending {
        Some(fresh_pass.as_ref().and_then(|p| p.as_ref()))
    } else {
        None
    };
    // The inos an OPEN cross-volume plan names — the block plane's
    // in-flight-registry role for the inode plane. A multi-commit plan is
    // exactly the window where a count and a name legitimately disagree,
    // and the plan's durable intent is what says one is in flight. One
    // bounded range per volume, empty on a healthy set.
    let intent_inos =
        if c10_judged.is_empty() && !c9_pending && !pending.iter().any(is_c10_dangling) {
            std::collections::HashSet::new()
        } else {
            open_intent_inos(ctx).await
        };

    // Phase C: per-suspect final verification.
    static EMPTY: once_cell::sync::Lazy<HashMap<u64, u32>> =
        once_cell::sync::Lazy::new(HashMap::new);
    // C8's fresh comparison, computed ONCE for the whole confirm pass
    // (the drift-gauge law, 2026-08-23): the per-suspect re-run paid one
    // full layout walk PER SUSPECT — 1,147 walks on the leg that found
    // this — and each raw pass used to feed the must-stay-0 gauge with
    // its transients. One pass judges every C8 suspect against one
    // consistent view.
    let mut c8_fresh: Option<Vec<(String, u64, u32, u32)>> = None;
    // C13's fresh census, ONCE per meta volume (`None` = the census
    // failed: no verdict for that volume's suspects, never guessed).
    let mut c13_fresh: HashMap<
        usize,
        Option<Vec<crate::meta_backend::kv::backend::OrphanImageExtent>>,
    > = HashMap::new();
    // C16's fresh census, ONCE per data volume (the same law).
    let mut c16_fresh: HashMap<
        u64,
        Option<Vec<crate::meta_backend::kv::shared_refs::SharedIndexDrift>>,
    > = HashMap::new();
    // C17's fresh view, ONCE for the pass (the same law; PR 7b Issue 14).
    let mut c17_fresh: Option<Option<C17Fresh>> = None;
    // C14 / C15's fresh custody census, ONCE per meta volume (the same
    // law): a recovery the ledger poll ran since the nomination clears
    // its C15 suspect here.
    let mut c1415_fresh: HashMap<usize, Option<crate::meta_backend::kv::backend::CustodyCensus>> =
        HashMap::new();
    for s in pending {
        if opts.cancel.load(Ordering::Relaxed) {
            break;
        }
        let verdict: Option<FsckFinding> = match &s.kind {
            SuspectKind::C2Leaked { vol, offset } => {
                let fresh = fresh.as_ref().expect("fresh walk ran");
                let refs = fresh.refs.get(vol).unwrap_or(&EMPTY);
                let still_tracked = alloc_of(vol)
                    .map(|a| a.refcount(*offset).is_some())
                    .unwrap_or(false);
                (still_tracked && !refs.contains_key(offset)).then(|| FsckFinding {
                    class: "C2".to_string(),
                    object: format!("{vol}:{offset}"),
                    evidence: "leaked block: allocated (tracked) with zero referencers, \
                               registry-cleared across two scan epochs"
                        .to_string(),
                    identity: Some(FindingId::C2Leaked {
                        vol: vol.clone(),
                        offset: *offset,
                    }),
                })
            }
            SuspectKind::C2Lost {
                vol,
                offset,
                ino,
                block_idx,
                mapping,
                why,
            } => {
                let by = format!("ino {ino} block {block_idx}");
                if vol == "?" {
                    // Unresolvable mapping: permanent by construction.
                    Some(FsckFinding {
                        class: "C2".to_string(),
                        object: by.clone(),
                        evidence: format!("lost block: {why}"),
                        identity: Some(FindingId::C2Lost {
                            vol: vol.clone(),
                            offset: *offset,
                            ino: *ino,
                            block_idx: *block_idx,
                            mapping: mapping.clone(),
                            unrepairable_shape: true,
                        }),
                    })
                } else {
                    // §5.6 normative order for the mover's src-free
                    // adversary: observe the ALLOCATOR state first, then
                    // re-verify the REFERENCE with a fresh per-ino layout
                    // read. A drain frees a source block only after every
                    // referencing publish is durable and visible, so a
                    // mapping still present AFTER the untracked
                    // observation is a genuine loss — while the
                    // moved-and-freed race always shows the NEW mapping
                    // at this re-read and clears (the leg-13
                    // drain-concurrent FP shape).
                    let now_tracked = alloc_of(vol)
                        .map(|a| a.refcount(*offset).is_some())
                        .unwrap_or(false);
                    let out_of_range = why.contains("capacity") || why.contains("aligned");
                    let violates = out_of_range || !now_tracked;
                    let still_referenced =
                        violates && current_mapping_present(ctx, *ino, *block_idx, mapping).await;
                    (still_referenced && violates).then(|| FsckFinding {
                        class: "C2".to_string(),
                        object: format!("{vol}:{offset}"),
                        evidence: format!("lost block ({by}): {why}"),
                        identity: Some(FindingId::C2Lost {
                            vol: vol.clone(),
                            offset: *offset,
                            ino: *ino,
                            block_idx: *block_idx,
                            mapping: mapping.clone(),
                            unrepairable_shape: out_of_range,
                        }),
                    })
                }
            }
            SuspectKind::C3Refcount { vol, offset, .. } => {
                let fresh = fresh.as_ref().expect("fresh walk ran");
                let refs = fresh.refs.get(vol).unwrap_or(&EMPTY);
                let expected = refs.get(offset).copied().unwrap_or(0);
                let actual = alloc_of(vol).and_then(|a| a.refcount(*offset));
                match actual {
                    Some(actual) if expected > 0 && actual != expected => Some(FsckFinding {
                        class: "C3".to_string(),
                        object: format!("{vol}:{offset}"),
                        evidence: format!(
                            "refcount {actual} != {expected} counted references \
                             (clone-aware walk; mover ledger cleared)"
                        ),
                        identity: Some(FindingId::C3Refcount {
                            vol: vol.clone(),
                            offset: *offset,
                        }),
                    }),
                    _ => None, // untracked/unreferenced shapes are C2's business
                }
            }
            SuspectKind::C8DurableRefDrift {
                vol,
                offset,
                durable,
                derived,
            } => {
                // Verify-before-report (KD-9): re-run the comparison ONCE
                // per confirm pass (memoized above) and keep the finding
                // only if THIS block still disagrees in it. A CONFIRMED
                // finding is what feeds the must-stay-0
                // `meta_kv_block_refs_drift` tripwire — the raw comparison
                // observes and convicts nothing (the drift-gauge law).
                if c8_fresh.is_none() {
                    c8_fresh = Some(
                        ctx.router
                            .backend_router
                            .verify_durable_block_refs(&ctx.meta)
                            .await
                            .unwrap_or_default(),
                    );
                }
                let fresh = c8_fresh.as_deref().unwrap_or(&[]);
                let chunk = ctx.router.backend_router.default_allocator.chunk_size();
                let idx = offset / chunk.max(1);
                let still_drifts = fresh.iter().any(|(v, i, _, _)| v == vol && *i == idx);
                // The comparison reads the durable scan and the layout walk
                // at DIFFERENT instants, so a block a publish lands on
                // between the two reads one reference apart — a transient
                // the two-epoch check excludes when the next publish moves
                // to another block, which it always did until the small-
                // file packer made ONE block the landing site of every
                // promotion in a batch (design-small-file-packing §5.3):
                // the open pack block re-drifts by a DIFFERENT commit on
                // both passes. An OPEN pack (the pack-open ledger) or a
                // block with a live in-flight owner is under publication
                // by construction — excused, counted, judged once it seals
                // or its owners deregister. Zero-FP in the report-only
                // direction; the quiesced mount-init verify is untouched.
                if still_drifts && alloc_of(vol).is_some_and(|alloc| pack_open(&alloc, *offset)) {
                    counters.pack_ledger_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                if still_drifts
                    && online
                    && alloc_of(vol).is_some_and(|alloc| alloc.inflight_contains(*offset))
                {
                    counters.inflight_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                still_drifts.then(|| {
                        crate::meta_backend::kv::META_KV_BLOCK_REFS_DRIFT
                            .fetch_add(1, Ordering::Relaxed);
                        FsckFinding {
                            class: "C8".to_string(),
                            object: format!("{vol}:{offset}"),
                            evidence: format!(
                                "durable block-reference drift: {durable} durable record(s)                              vs {derived} counted layout reference(s), stable across                              both scan epochs — the durable ledger and the layouts that                              justify it diverged"
                            ),
                            identity: Some(FindingId::C8DurableRefDrift {
                                vol: vol.clone(),
                                offset: *offset,
                            }),
                        }
                    })
            }
            SuspectKind::C6Drift { vol, .. } => {
                let Some(alloc) = alloc_of(vol) else {
                    continue;
                };
                let used = alloc
                    .highest_block_index()
                    .saturating_sub(alloc.free_blocks_count());
                let tracked = alloc.tracked_offsets().len() as u64;
                (used != tracked).then(|| FsckFinding {
                    class: "C6".to_string(),
                    object: vol.clone(),
                    evidence: format!(
                        "capacity census drift: used-blocks accounting {used} vs \
                         {tracked} tracked refcounted blocks, stable across both \
                         scan epochs"
                    ),
                    identity: Some(FindingId::C6Drift { vol: vol.clone() }),
                })
            }
            SuspectKind::C1Walk {
                vol,
                tree,
                slot,
                cursor,
                ..
            } => {
                // Re-attempt the read (a transient I/O error clears) —
                // the scan's own page shape.
                let kv = &ctx.meta.volumes[*vol];
                let unit = match slot {
                    Some(s) => C1Unit::Slot(*s),
                    None => C1Unit::Kind(*tree),
                };
                match c1_page(kv, unit, cursor).await {
                    Err(e) => Some(FsckFinding {
                        class: "C1".to_string(),
                        object: match slot {
                            Some(s) => format!("vol{vol}/slot{s}/cursor{}", hex(cursor)),
                            None => format!("vol{vol}/tree{tree}/cursor{}", hex(cursor)),
                        },
                        evidence: format!("tree walk failed (checksum/undecodable node): {e}"),
                        identity: Some(FindingId::C1Torn {
                            vol: *vol,
                            tree: *tree,
                            slot: *slot,
                            cursor_hex: hex(cursor),
                        }),
                    }),
                    Ok(_) => None,
                }
            }
            SuspectKind::C1RawKey {
                vol,
                slot,
                key,
                why,
            } => {
                let kv = &ctx.meta.volumes[*vol];
                match kv.slot_tree_lookup_raw(*slot, key).await {
                    Ok(Some(_)) => Some(FsckFinding {
                        class: "C1".to_string(),
                        object: format!("vol{vol}/slot{slot}/rawkey{}", hex(key)),
                        evidence: format!("checksum-valid semantic damage: {why}"),
                        identity: Some(FindingId::C1RawKey {
                            vol: *vol,
                            slot: *slot,
                            key_hex: hex(key),
                        }),
                    }),
                    Ok(None) => None, // vanished (a live delete / a repair)
                    Err(e) => Some(FsckFinding {
                        class: "C1".to_string(),
                        object: format!("vol{vol}/slot{slot}/rawkey{}", hex(key)),
                        evidence: format!("record unreadable at re-check: {e}"),
                        identity: Some(FindingId::C1RawKey {
                            vol: *vol,
                            slot: *slot,
                            key_hex: hex(key),
                        }),
                    }),
                }
            }
            SuspectKind::C13OrphanImageExtent {
                vol,
                appender,
                extent,
            } => {
                // ONE fresh census per volume for the whole confirm pass
                // (the C8 shape): the census walks the volume's interior
                // population, so a per-suspect re-run would pay it per
                // orphan. Under the backend's SMO + mint serialization
                // again, so the settle + this re-read together clear a
                // publication that landed since the nomination.
                let fresh = match c13_fresh.entry(*vol) {
                    std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                    std::collections::hash_map::Entry::Vacant(v) => {
                        let kv = &ctx.meta.volumes[*vol];
                        v.insert(kv.c13_orphan_image_extents().await.ok())
                    }
                };
                let still = fresh.as_ref().is_some_and(|o| {
                    o.iter()
                        .any(|x| x.appender == *appender && x.extent == *extent)
                });
                still.then(|| FsckFinding {
                    class: "C13".to_string(),
                    object: format!("vol{vol}/appender{appender}/extent{extent}"),
                    evidence: format!(
                        "orphan image extent: claimed inside appender {appender}'s grant, reached \
                         by no slot-tree root and parked by no pending-free at two censuses \
                         under the volume's SMO serialization (an unpublished root swap's \
                         successor, or a grant remainder the page could not name)"
                    ),
                    identity: Some(FindingId::C13OrphanImageExtent {
                        vol: *vol,
                        appender: *appender,
                        extent: *extent,
                    }),
                })
            }
            SuspectKind::C16SharedIndexDrift {
                vol_tag,
                block_idx,
                owner_ino,
                block_index,
                flag_side,
            } => {
                // Verify-before-report: ONE fresh census per data volume for
                // the confirm pass (the C8/C13 shape); a drift a clone's
                // steps 1–3 were mid-way through at the nomination has
                // closed by now and clears.
                let fresh = match c16_fresh.entry(*vol_tag) {
                    std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                    std::collections::hash_map::Entry::Vacant(v) => v.insert(
                        crate::meta_backend::kv::shared_refs::shared_index_drift(
                            &ctx.meta, *vol_tag,
                        )
                        .await
                        .ok(),
                    ),
                };
                let still = fresh.as_ref().is_some_and(|d| {
                    d.iter().any(|x| {
                        let (r, side) = match x {
                            crate::meta_backend::kv::shared_refs::SharedIndexDrift::FlagWithoutEntry(r) => (r, true),
                            crate::meta_backend::kv::shared_refs::SharedIndexDrift::EntryWithoutFlag(r) => (r, false),
                        };
                        side == *flag_side
                            && r.block_idx == *block_idx
                            && r.owner_ino == *owner_ino
                            && r.block_index == *block_index
                    })
                });
                if !still {
                    counters.suspects_cleared += 1;
                    continue;
                }
                counters.shared_index_drift += 1;
                Some(FsckFinding {
                    class: "C16".to_string(),
                    object: format!("{vol_tag:#x}:{block_idx}/ino{owner_ino}#{block_index}"),
                    evidence: if *flag_side {
                        format!(
                            "shared-index drift: ino {owner_ino}'s reference to block \
                             {block_idx} (map index {block_index}) carries SHARED but the index \
                             home names no entry for it, stable across two censuses — a clone \
                             that died between its MarkShared and its ShareBlock, or a lost \
                             index entry. REPORT-ONLY: the block's terminal free consults the \
                             index and frees it when nothing remains"
                        )
                    } else {
                        format!(
                            "shared-index drift: the index home names ino {owner_ino} on block \
                             {block_idx} (map index {block_index}) but that ino holds no SHARED \
                             reference to it, stable across two censuses — a cloner that died \
                             after its ShareBlock and before its publish. REPORT-ONLY: the \
                             home's GC arm drops the entry at the block's next release"
                        )
                    },
                    identity: Some(FindingId::C16SharedIndexDrift {
                        vol_tag: *vol_tag,
                        block_idx: *block_idx,
                        owner_ino: *owner_ino,
                        block_index: *block_index,
                        flag_side: *flag_side,
                    }),
                })
            }
            // C17 (PR 7b): verify-before-report — the shape must hold on a
            // FRESH read of its directory's markers and stripes; a flip or
            // a migration mid-way at the nomination has settled by now and
            // clears.
            SuspectKind::C17StripeInconsistency(shape) => {
                if c17_fresh.is_none() {
                    c17_fresh = Some(C17Fresh::build(ctx).await);
                }
                let holds = match c17_fresh.as_mut().and_then(|f| f.as_mut()) {
                    Some(fresh) => fresh.holds(ctx, shape).await,
                    None => false,
                };
                if !holds {
                    counters.suspects_cleared += 1;
                    continue;
                }
                counters.stripe_findings += 1;
                Some(FsckFinding {
                    class: "C17".to_string(),
                    object: format!("dir{}/stripe{}", shape.dir(), shape.stripe()),
                    evidence: format!(
                        "{} — stable across two censuses. REPORT-ONLY (design-symmetric-\
                         metadata §5.6.5, the C8 posture)",
                        shape.evidence()
                    ),
                    identity: Some(FindingId::C17StripeInconsistency {
                        dir: shape.dir(),
                        stripe: shape.stripe(),
                        shape: shape.label().to_string(),
                        name: shape.name(),
                    }),
                })
            }

            SuspectKind::C14SlotCustodyConflict {
                vol,
                slot,
                appender_a,
                appender_b,
            } => {
                let fresh = match c1415_fresh.entry(*vol) {
                    std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                    std::collections::hash_map::Entry::Vacant(v) => {
                        let kv = &ctx.meta.volumes[*vol];
                        let vol0 = crate::meta_backend::kv::backend::recovery::vol0_of(&ctx.meta)
                            .map(|(_, v)| v);
                        v.insert(kv.slot_custody_census(vol0).await.ok())
                    }
                };
                let still = fresh.as_ref().is_some_and(|c| {
                    c.conflicts.iter().any(|(s, a, b)| {
                        *s == *slot
                            && ((*a == *appender_a && *b == *appender_b)
                                || (*a == *appender_b && *b == *appender_a))
                    })
                });
                if !still {
                    counters.suspects_cleared += 1;
                    continue;
                }
                counters.slot_custody_conflicts += 1;
                Some(FsckFinding {
                    class: "C14".to_string(),
                    object: format!("vol{vol}/slot{slot}"),
                    evidence: format!(
                        "slot custody conflict: forest slot {slot} is attested Live by appender \
                         {appender_a}'s page and held by appender {appender_b} (a second Live \
                         page, or tree 0's lease) at the same generation, stable across two \
                         censuses — a shape no grant or release writes. The mount REFUSES on \
                         it; the remedy is the operator's attestation: `squeezefs appender \
                         clear <sqmeta-uri> <id>` for the page that is dead"
                    ),
                    identity: Some(FindingId::C14SlotCustodyConflict {
                        vol: *vol,
                        slot: *slot,
                        appender_a: *appender_a,
                        appender_b: *appender_b,
                    }),
                })
            }
            SuspectKind::C15UnrecoveredAppender {
                vol,
                appender,
                node_token,
                mount_slot,
                window_entries,
            } => {
                let fresh = match c1415_fresh.entry(*vol) {
                    std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                    std::collections::hash_map::Entry::Vacant(v) => {
                        let kv = &ctx.meta.volumes[*vol];
                        let vol0 = crate::meta_backend::kv::backend::recovery::vol0_of(&ctx.meta)
                            .map(|(_, v)| v);
                        v.insert(kv.slot_custody_census(vol0).await.ok())
                    }
                };
                let still = fresh.as_ref().is_some_and(|c| {
                    c.unrecovered.iter().any(|(id, ident, _)| {
                        *id == *appender
                            && ident.node_token == *node_token
                            && ident.mount_slot == *mount_slot
                    })
                });
                if !still {
                    counters.suspects_cleared += 1;
                    continue;
                }
                counters.unrecovered_appenders += 1;
                Some(FsckFinding {
                    class: "C15".to_string(),
                    object: format!("vol{vol}/appender{appender}"),
                    evidence: format!(
                        "un-recovered appender: appender {appender}'s page (node {node_token:#018x}, \
                         mount slot {mount_slot:#x}) is Live or Recovering while volume 0's death \
                         ledger names its identity dead, with {window_entries} acked entries in \
                         its ring window or slots still leased to it, stable across two censuses. \
                         The §5.9 recovery replays the window into its slot trees, records every \
                         leaf's tail, unleases its slots and returns its grant — the volume's \
                         manager runs it at its ledger poll and the mount path runs it before \
                         serving"
                    ),
                    identity: Some(FindingId::C15UnrecoveredAppender {
                        vol: *vol,
                        appender: *appender,
                        node_token: *node_token,
                        mount_slot: *mount_slot,
                        window_entries: *window_entries,
                    }),
                })
            }
            SuspectKind::C1Record {
                vol,
                tree,
                key,
                why,
            } => {
                let kv = &ctx.meta.volumes[*vol];
                // Final check under the owning ino's 4a lease where the
                // key names one (online).
                let ino = owning_ino(*tree, key);
                let _lease = match (online, ino) {
                    (true, Some(local)) => Some(kv.dlm().lock_inode_exclusive(local).await),
                    _ => None,
                };
                match kv.lookup_kind(*tree, key).await {
                    Ok(Some(_)) => Some(FsckFinding {
                        class: "C1".to_string(),
                        object: format!("vol{vol}/tree{tree}/key{}", hex(key)),
                        evidence: format!("checksum-valid semantic damage: {why}"),
                        identity: Some(FindingId::C1Semantic {
                            vol: *vol,
                            tree: *tree,
                            key_hex: hex(key),
                        }),
                    }),
                    Ok(None) => None, // record vanished (live delete)
                    Err(e) => Some(FsckFinding {
                        class: "C1".to_string(),
                        object: format!("vol{vol}/tree{tree}/key{}", hex(key)),
                        evidence: format!("record unreadable at re-check: {e}"),
                        identity: Some(FindingId::C1Semantic {
                            vol: *vol,
                            tree: *tree,
                            key_hex: hex(key),
                        }),
                    }),
                }
            }
            SuspectKind::C4Orphan { dir, key, ino } => {
                let (vol_idx, local) = ctx.meta.route_ino(*ino);
                let _lease = if online {
                    Some(
                        ctx.meta.volumes[vol_idx]
                            .dlm()
                            .lock_inode_exclusive(local)
                            .await,
                    )
                } else {
                    None
                };
                // Custody still present?
                let still_present =
                    match crate::cache::nvme::scan_live_staged_custody(dir, CUSTODY_SCAN_MAX).await
                    {
                        Ok(keys) => keys.iter().any(|k| k == key),
                        Err(_) => false,
                    };
                // Re-read through the RAW per-volume backend: the routed
                // getattr takes its own shared 4a lease and would
                // self-deadlock against the exclusive lease held above.
                let still_missing = match ctx.meta.volumes[vol_idx].getattr(local).await {
                    Ok(inode) => inode.nlink == 0,
                    Err(e) if is_not_found(&e) => true,
                    Err(_) => false,
                };
                (still_present && still_missing).then(|| FsckFinding {
                    class: "C4".to_string(),
                    object: key.clone(),
                    evidence: format!(
                        "orphan staged record in {}: ino {ino} has no live inode meta",
                        dir.display()
                    ),
                    identity: Some(FindingId::C4Orphan {
                        dir: dir.clone(),
                        key: key.clone(),
                        ino: *ino,
                    }),
                })
            }
            SuspectKind::C9Unreferenced {
                ino,
                nlink,
                size,
                blocks,
            } => {
                // An ino an OPEN cross-volume plan names is in flight by
                // definition (Issue 10 — C10's guard, C9's too): the plan
                // that names it is still to land, and its intent is what
                // says so.
                if intent_inos.contains(ino) {
                    counters.unreferenced_intent_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                // Verify under the ino's exclusive 4a lease (online):
                // the record is still live, still prior-era, and the
                // FRESH dentry pass still found no name for it. The raw
                // per-volume read is deliberate — the routed `getattr`
                // takes its own shared 4a lease and would self-deadlock
                // against the exclusive one held here (the C4 lesson).
                let (vol_idx, local) = ctx.meta.route_ino(*ino);
                let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
                    continue;
                };
                let _lease = if online {
                    Some(kv.dlm().lock_inode_exclusive(local).await)
                } else {
                    None
                };
                let still_live = matches!(
                    kv.read_inode_value_routed(local).await,
                    Ok(Some(v)) if v.nlink > 0
                );
                let still_prior_era = kv.minted_in_prior_era(local);
                let still_unnamed = match &fresh_refs {
                    Some(Some(pass)) => !pass.refs.contains(*ino),
                    // Incomplete fresh pass ⇒ no verdict (clears).
                    Some(None) => false,
                    // Offline: nothing is in flight by definition.
                    None => true,
                };
                (still_live && still_prior_era && still_unnamed).then(|| FsckFinding {
                    class: "C9".to_string(),
                    object: format!("ino {ino}"),
                    evidence: format!(
                        "unreferenced inode: no dentry names ino {ino} (nlink {nlink},                          size {size} B, {blocks} block reference(s)); the record was                          minted in a PRIOR writer era and stayed unnamed across both                          dentry passes — a crashed cross-volume create leaves this shape                          with 0 blocks, pre-S3.5 cross-volume link/unlink damage with its                          blocks still attached"
                    ),
                    identity: Some(FindingId::C9Unreferenced { ino: *ino }),
                })
            }
            // C10's count + zero-count arms share ONE ladder, and the
            // DIRECTION is a property of the verified numbers rather than
            // of the nomination — so neither arm can drift from the
            // other's guards.
            SuspectKind::C10NlinkMismatch { ino, .. }
            | SuspectKind::C10ZeroNlinkNamed { ino, .. } => {
                // Guard: an ino an OPEN cross-volume plan names is in
                // flight by definition — a multi-commit plan is exactly
                // the window where the count and the names disagree.
                if intent_inos.contains(ino) {
                    counters.nlink_transient_cleared += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                let (vol_idx, local) = ctx.meta.route_ino(*ino);
                let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
                    continue;
                };
                // The raw per-volume read under the exclusive 4a lease —
                // the routed `getattr` would self-deadlock against it (the
                // C4 lesson), and the fold-free value is what the census
                // compared.
                let _lease = if online {
                    Some(kv.dlm().lock_inode_exclusive(local).await)
                } else {
                    None
                };
                let Ok(Some(val)) = kv.read_inode_value_routed(local).await else {
                    // The record is gone: whatever this was, it is now the
                    // dangling-name shape and the NEXT run owns it.
                    counters.nlink_transient_cleared += 1;
                    counters.suspects_cleared += 1;
                    continue;
                };
                // The witness's second half. Any change to the record
                // across the fresh pass means an op that could move this
                // ino's name count committed inside the pass, which is the
                // only way the pass's read-over-time skew can produce a
                // number — so the suspect clears.
                if online && c10_witness.get(ino) != Some(&(val.nlink, val.ctime)) {
                    counters.nlink_transient_cleared += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                // Every verdict's numerator: the DEDUPED distinct names
                // the fresh pass collected. An incomplete pass records no
                // verdict.
                let names = match &fresh_pass {
                    Some(Some(pass)) => pass.collected(*ino).len() as u32,
                    _ => {
                        counters.suspects_cleared += 1;
                        continue;
                    }
                };
                let is_dir = val.mode & libc::S_IFMT == libc::S_IFDIR;
                if names == 0 {
                    // Named by nobody is C9's object, never C10's.
                    counters.suspects_cleared += 1;
                    continue;
                }
                if val.nlink == names || (is_dir && val.nlink > 0) {
                    // Healed, or the record-count nomination was a slot
                    // migration's duplicate that the distinct-name dedupe
                    // resolved — and a live directory's nlink is never its
                    // name count (`.` and every child's `..`).
                    counters.nlink_transient_cleared += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                let object = format!("ino {ino}");
                if val.nlink == 0 {
                    counters.nlink_zero_named += 1;
                    Some(FsckFinding {
                        class: "C10".to_string(),
                        object,
                        evidence: format!(
                            "nlink 0 while {names} dentry name(s) still reference ino \
                             {ino}: DATA-LOSS RISK — the live-nlink skip that keeps \
                             reclaim off a reachable inode no longer applies, so an \
                             ordinary reclaim may destroy an inode a path still \
                             resolves. A cross-volume unlink whose count step committed \
                             and whose name step did not leaves exactly this shape{}",
                            if is_dir {
                                " (this inode is a DIRECTORY: its count is 2 + \
                                 subdirectories, which this class does not compute, so \
                                 repair refuses it)"
                            } else {
                                ""
                            }
                        ),
                        identity: Some(FindingId::C10ZeroNlinkNamed { ino: *ino }),
                    })
                } else if val.nlink > names {
                    counters.nlink_mismatch_high += 1;
                    Some(FsckFinding {
                        class: "C10".to_string(),
                        object,
                        evidence: format!(
                            "nlink {} exceeds the {names} dentry name(s) that reference \
                             ino {ino}: the inode and every block it owns can never be \
                             reclaimed — a leak that reads as healthy. A cross-volume \
                             link whose count step committed and whose name step did \
                             not leaves exactly this shape",
                            val.nlink
                        ),
                        identity: Some(FindingId::C10NlinkTooHigh { ino: *ino }),
                    })
                } else {
                    counters.nlink_mismatch_low += 1;
                    Some(FsckFinding {
                        class: "C10".to_string(),
                        object,
                        evidence: format!(
                            "nlink {} is below the {names} dentry name(s) that reference \
                             ino {ino}: DATA-LOSS RISK — once ordinary unlinks drive \
                             this count to 0, reclaim is entitled to destroy an inode a \
                             live path still resolves",
                            val.nlink
                        ),
                        identity: Some(FindingId::C10NlinkTooLow { ino: *ino }),
                    })
                }
            }
            SuspectKind::C10Dangling {
                vol,
                key,
                parent,
                name,
                child_ino,
                ..
            } => {
                // Same in-flight guard (a plan's `MintInode` may not have
                // landed yet), then: the dentry record must still exist
                // VERBATIM and still name this ino, and the ino must still
                // have no record. The dentry lock class is the PARENT's
                // (`owning_ino`'s rule for `TREE_DENTRIES`).
                if intent_inos.contains(child_ino) || intent_inos.contains(parent) {
                    counters.nlink_transient_cleared += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    continue;
                };
                let local_parent =
                    crate::meta_backend::kv::record::decode_dentry_key(key).map(|(p, _, _)| p);
                let _lease = match (online, local_parent) {
                    (true, Ok(p)) => Some(kv.dlm().lock_inode_exclusive(p).await),
                    _ => None,
                };
                let still_named = match kv
                    .lookup_kind(crate::meta_backend::kv::record::TREE_DENTRIES, key)
                    .await
                {
                    Ok(Some(v)) => crate::meta_backend::kv::record::DentryValue::decode(&v)
                        .map(|d| d.child_ino == *child_ino)
                        .unwrap_or(false),
                    _ => false,
                };
                let (child_vol, child_local) = ctx.meta.route_ino(*child_ino);
                let still_missing = match ctx.meta.volumes.get(child_vol) {
                    Some(child_kv) => {
                        matches!(
                            child_kv.read_inode_value_routed(child_local).await,
                            Ok(None)
                        )
                    }
                    None => false,
                };
                if !(still_named && still_missing) {
                    counters.nlink_transient_cleared += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                counters.dangling_dentries += 1;
                Some(FsckFinding {
                    class: "C10".to_string(),
                    object: format!("dentry {parent}/{name}"),
                    evidence: format!(
                        "dangling dentry: '{name}' in parent {parent} names ino \
                         {child_ino}, which has no inode record — the name resolves to \
                         nothing (a lookup finds it and every stat of it fails). A \
                         cross-volume unlink whose count step committed and whose name \
                         step did not leaves exactly this shape; S3.5 made such a name \
                         removable, and this class is what finds one"
                    ),
                    identity: Some(FindingId::C10DanglingDentry {
                        vol: *vol,
                        key_hex: hex(key),
                        child_ino: *child_ino,
                    }),
                })
            }
            // C11 (design-kvmap-block-map-tree §3 fsck + A3): both
            // shields, in order — the registry BEFORE the lease (a live
            // train holds the ino's 4a across sweep → chunks → flip, so
            // waiting on it here could park the pass for a whole PB-class
            // migration), then the exclusive 4a re-check (a lease held
            // here brackets out any train), then fresh state reads.
            SuspectKind::C11OrphanMapRecords { vol, local_ino } => {
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    continue;
                };
                if kv.crossing_in_flight(*local_ino) {
                    counters.crossing_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                let _lease = if online {
                    Some(kv.dlm().lock_inode_exclusive(*local_ino).await)
                } else {
                    None
                };
                if kv.crossing_in_flight(*local_ino) {
                    counters.crossing_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                let records = kvmap_record_count(kv, *local_ino).await;
                if records == 0 {
                    // The A1/unlink sweep won the race: nothing stands.
                    counters.suspects_cleared += 1;
                    continue;
                }
                let live = match kv.read_inode_value_routed(*local_ino).await {
                    Ok(v) => v.is_some(),
                    Err(_) => {
                        counters.suspects_cleared += 1; // no verdict
                        continue;
                    }
                };
                if live && head_is_kvmap(&kvmap_head_of(kv, *local_ino).await) {
                    // The crossing completed between passes: the head now
                    // names the tree.
                    counters.suspects_cleared += 1;
                    continue;
                }
                counters.map_orphan_records += records;
                Some(FsckFinding {
                    class: "C11".to_string(),
                    object: format!("vol{vol} ino {local_ino}"),
                    evidence: format!(
                        "{records} orphan block-map record(s): owner ino {local_ino} \
                         (volume-local) {} — a crashed crossing/unlink's residue, \
                         invisible to reads (the head is the truth) and reclaimed by \
                         the next crossing's A1 sweep. REPORT-ONLY (design A3): a false \
                         quarantine would hole a live crossing",
                        if live {
                            "is live but its head is not kvmap-class"
                        } else {
                            "has no live inode record"
                        }
                    ),
                    identity: Some(FindingId::C11OrphanMapRecords {
                        vol: *vol,
                        ino: *local_ino,
                        records,
                    }),
                })
            }
            SuspectKind::C11EmptyKvmapHead {
                vol,
                local_ino,
                size,
            } => {
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    continue;
                };
                if kv.crossing_in_flight(*local_ino) {
                    counters.crossing_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                let _lease = if online {
                    Some(kv.dlm().lock_inode_exclusive(*local_ino).await)
                } else {
                    None
                };
                if kv.crossing_in_flight(*local_ino) {
                    counters.crossing_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                // Fresh state, all four legs: still live, head still a
                // sweep-less kvmap sentinel, size still nonzero, tree
                // still empty. Any leg moved ⇒ cleared.
                let live = matches!(kv.read_inode_value_routed(*local_ino).await, Ok(Some(v)) if v.nlink > 0);
                let head = kvmap_head_of(kv, *local_ino).await;
                let still_empty_sentinel = head
                    .as_ref()
                    .and_then(|l| l.block_map_id.as_deref())
                    .and_then(|id| crate::meta_backend::kv::block_map::parse_kvmap_head(id).ok())
                    .is_some_and(|h| h.sweep_cursor.is_none())
                    && head.as_ref().is_some_and(|l| l.size > 0);
                let still_no_records = matches!(
                    kv.block_map_range(*local_ino, 0, 1).await,
                    Ok(page) if page.is_empty()
                );
                if !(live && still_empty_sentinel && still_no_records) {
                    counters.suspects_cleared += 1;
                    continue;
                }
                counters.map_empty_heads += 1;
                Some(FsckFinding {
                    class: "C11".to_string(),
                    object: format!("vol{vol} ino {local_ino}"),
                    evidence: format!(
                        "kvmap head with ZERO tree records at {size} B of declared size: \
                         every read of ino {local_ino} (volume-local) resolves to holes \
                         while the head claims mapped data — the fully-empty coverage \
                         mismatch (the size-vs-sparse ambiguity keeps partial coverage \
                         out of scope). REPORT-ONLY (design A3): restating the map would \
                         fabricate data"
                    ),
                    identity: Some(FindingId::C11EmptyKvmapHead {
                        vol: *vol,
                        ino: *local_ino,
                    }),
                })
            }
            SuspectKind::C11RunForeignShadow {
                vol,
                local_ino,
                run_start,
                idx,
            } => {
                // The (a)/(b) shield ladder verbatim: registry, lease,
                // registry again, then a FRESH evidence re-scan — a
                // publish between passes legitimately re-canonicalizes
                // the span (the publish train is the coalescer), which
                // clears the suspect.
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    continue;
                };
                if kv.crossing_in_flight(*local_ino) {
                    counters.crossing_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                let _lease = if online {
                    Some(kv.dlm().lock_inode_exclusive(*local_ino).await)
                } else {
                    None
                };
                if kv.crossing_in_flight(*local_ino) {
                    counters.crossing_exempted += 1;
                    counters.suspects_cleared += 1;
                    continue;
                }
                if !kvmap_run_foreign_shadows(kv, *local_ino)
                    .await
                    .contains(&(*run_start, *idx))
                {
                    counters.suspects_cleared += 1;
                    continue;
                }
                counters.map_run_foreign_shadows += 1;
                Some(FsckFinding {
                    class: "C11".to_string(),
                    object: format!("vol{vol} ino {local_ino}"),
                    evidence: format!(
                        "cross-volume point record at index {idx} strictly inside the run \
                         at [{run_start}, …): the point supersedes by the §2 read law (a \
                         legal shape same-volume), but a DIFFERENT-volume override inside \
                         a straight run is design §12's report-only sanity signal — a \
                         mover/claims train left an unusual span. REPORT-ONLY: the point \
                         is presumed the truth; the next full publish re-canonicalizes"
                    ),
                    identity: Some(FindingId::C11RunForeignShadow {
                        vol: *vol,
                        ino: *local_ino,
                        run_start: *run_start,
                        idx: *idx,
                    }),
                })
            }
            SuspectKind::C12Overlap { vol, offset, a, b } => {
                // The census read the two layouts at different instants:
                // between them the block may have been freed, recycled as
                // a fresh pack and refilled, so the "intersection" names
                // two lifetimes. Post-settle the exemptions are re-read,
                // then BOTH layouts are re-read under their inos'
                // exclusive 4a leases (one canonical acquisition — a
                // layout publish takes the same lease, so the pair is a
                // consistent cut) and the finding stands only if the
                // CURRENT mappings still name this block and still
                // intersect at different `off`.
                if let Some(alloc) = alloc_of(vol) {
                    if online && alloc.inflight_contains(*offset) {
                        counters.inflight_exempted += 1;
                        counters.suspects_cleared += 1;
                        continue;
                    }
                    if pack_open(&alloc, *offset) {
                        counters.pack_ledger_exempted += 1;
                        counters.suspects_cleared += 1;
                        continue;
                    }
                }
                let fresh = current_tenant_windows(
                    ctx,
                    online,
                    &[(a.ino, a.block_idx), (b.ino, b.block_idx)],
                )
                .await;
                let (Some((va, fa)), Some((vb, fb))) = (&fresh[0], &fresh[1]) else {
                    counters.suspects_cleared += 1;
                    continue;
                };
                let same_block = va.0 == *vol
                    && va.1 == *offset
                    && vb.0 == *vol
                    && vb.1 == *offset
                    && va.2 == vb.2;
                if !same_block || !fa.intersects_at_different_off(fb) {
                    counters.suspects_cleared += 1;
                    continue;
                }
                counters.tenant_overlap_findings += 1;
                Some(FsckFinding {
                    class: "C12".to_string(),
                    object: format!("{vol}:{offset}"),
                    evidence: format!(
                        "tenant windows on one block intersect at DIFFERENT offsets: ino {} \
                         block {} '{}' → [{}, {}) vs ino {} block {} '{}' → [{}, {}) — a slot \
                         minted inside another slot (design-small-file-packing §5.9), \
                         reproduced on a fresh re-read of both layouts under their leases. \
                         REPORT-ONLY: at least one tenant is wrong and nothing on the volume \
                         says which; quarantining both would destroy the right one",
                        fa.ino,
                        fa.block_idx,
                        fa.mapping,
                        fa.off,
                        fa.end,
                        fb.ino,
                        fb.block_idx,
                        fb.mapping,
                        fb.off,
                        fb.end
                    ),
                    identity: Some(FindingId::C12Overlap {
                        vol: vol.clone(),
                        offset: *offset,
                        ino_a: fa.ino,
                        block_idx_a: fa.block_idx,
                        ino_b: fb.ino,
                        block_idx_b: fb.block_idx,
                    }),
                })
            }
            SuspectKind::C12Overrun {
                vol,
                offset,
                ino,
                block_idx,
                mapping,
                why,
            } => {
                if let Some(alloc) = alloc_of(vol) {
                    if online && alloc.inflight_contains(*offset) {
                        counters.inflight_exempted += 1;
                        counters.suspects_cleared += 1;
                        continue;
                    }
                    if pack_open(&alloc, *offset) {
                        counters.pack_ledger_exempted += 1;
                        counters.suspects_cleared += 1;
                        continue;
                    }
                }
                // Verbatim presence on the fresh read under the ino's lease
                // is the whole re-check: the same string decodes the same
                // way, so the violation reproduces iff the mapping does.
                let fresh = current_tenant_mappings(ctx, online, &[(*ino, *block_idx)]).await;
                let still_present = fresh[0].as_ref().is_some_and(|m| m.mapping == *mapping);
                if !still_present {
                    counters.suspects_cleared += 1;
                    continue;
                }
                counters.tenant_overlap_findings += 1;
                Some(FsckFinding {
                    class: "C12".to_string(),
                    object: format!("{vol}:{offset}"),
                    evidence: format!(
                        "tenant window law violated by ino {ino} block {block_idx} '{mapping}': \
                         {why} — every read of it refuses (EIO, `packed_mapping_refusals`) \
                         while the census counts its base reference (design-small-file-packing \
                         §5.9 C12Overrun). REPORT-ONLY: the base block IS referenced; only the \
                         window is unreadable, and the right repair is the owner's, not a guess"
                    ),
                    identity: Some(FindingId::C12Overrun {
                        vol: vol.clone(),
                        offset: *offset,
                        ino: *ino,
                        block_idx: *block_idx,
                        mapping: mapping.clone(),
                    }),
                })
            }
            SuspectKind::C5Generation { dir, why } => {
                // Re-read the marker (a live restamp clears).
                let expected = ctx.expected_generation.as_deref().unwrap_or("");
                match crate::cache::nvme::read_staging_generation_marker(dir).await {
                    Ok(Some(found))
                        if crate::writer_scope::marker_is_rebindable(&found, expected) =>
                    {
                        None
                    }
                    _ => Some(FsckFinding {
                        class: "C5".to_string(),
                        object: dir.display().to_string(),
                        evidence: why.clone(),
                        identity: Some(FindingId::C5Staging { dir: dir.clone() }),
                    }),
                }
            }
        };
        match verdict {
            Some(f) => findings.push(f),
            None => counters.suspects_cleared += 1,
        }
    }
    Ok(())
}

fn is_c10_dangling(s: &Suspect) -> bool {
    matches!(s.kind, SuspectKind::C10Dangling { .. })
}

/// Every ino an **open** cross-volume intent's steps name — C10's
/// in-flight exemption (the block plane's `inflight_contains` role for the
/// inode plane).
///
/// A multi-commit plan is precisely the window where an inode's count and
/// its names legitimately disagree, and `execute` retires the intent
/// SYNCHRONOUSLY before releasing its guards (§4.10a), so an intent that
/// still exists means a plan is in flight or a crash left one for the next
/// mount to roll forward. Both parents and children are exempted: a plan's
/// dentry steps move the parent's names and the child's count. One bounded
/// range scan per volume, empty on a healthy set — and a decode failure
/// exempts nothing while a SCAN failure exempts nothing either, which is
/// safe here only because every other C10 guard still applies.
async fn open_intent_inos(ctx: &FsckCtx) -> std::collections::HashSet<u64> {
    use crate::meta_backend::crossvol_tx::{IntentRecord, XvStep};
    let mut out = std::collections::HashSet::new();
    for kv in &ctx.meta.volumes {
        let Ok(intents) = kv.xv_scan_intents().await else {
            continue;
        };
        for (_tx_id, image) in intents {
            let Ok(record) = IntentRecord::decode(&image) else {
                continue; // an unreadable intent refuses the next MOUNT; not ours to judge
            };
            for step in &record.steps {
                out.insert(step.home_ino());
                match step {
                    XvStep::RemoveDentry {
                        parent,
                        expect_child,
                        ..
                    } => {
                        out.insert(*parent);
                        out.insert(*expect_child);
                    }
                    XvStep::InsertDentry { parent, child, .. } => {
                        out.insert(*parent);
                        out.insert(*child);
                    }
                    XvStep::SetNlink { ino, .. }
                    | XvStep::TouchCtime { ino, .. }
                    | XvStep::MintInode { ino, .. }
                    | XvStep::CreateInode { ino, .. } => {
                        out.insert(*ino);
                    }
                }
            }
        }
    }
    out
}

/// The local ino a record key belongs to (per tree schema).
fn owning_ino(tree: u8, key: &[u8]) -> Option<u64> {
    use crate::meta_backend::kv::record::{
        decode_dentry_key, decode_inode_key, decode_xattr_key, TREE_DENTRIES, TREE_INODES,
        TREE_XATTRS,
    };
    match tree {
        t if t == TREE_INODES => decode_inode_key(key).ok(),
        t if t == TREE_DENTRIES => decode_dentry_key(key).ok().map(|(parent, _, _)| parent),
        t if t == TREE_XATTRS => decode_xattr_key(key).ok().map(|(ino, _, _)| ino),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// C7: the data scrub (KD-17)
// ---------------------------------------------------------------------------

async fn scrub_c7(
    ctx: &FsckCtx,
    opts: &FsckOptions,
    census: &CensusOut,
    counters: &mut FsckCounters,
    findings: &mut Vec<FsckFinding>,
) {
    let crypto = ctx.router.get_crypto().clone();
    let block_size = ctx
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    // Device-bound sequential order: sort by (volume, offset).
    let mut work: Vec<&MappingRef> = census.mappings.iter().collect();
    work.sort_by_key(|m| clean_key(&m.mapping));
    let mut t0 = std::time::Instant::now();
    let mut in_batch = 0usize;
    for m in work {
        if opts.cancel.load(Ordering::Relaxed) {
            break;
        }
        if m.damaged {
            // §5.6a quarantined mapping: already isolated (reads EIO,
            // block preserved for forensics) — never re-scrubbed, never
            // re-reported.
            continue;
        }
        match verify_stored_block(ctx, &crypto, m, block_size).await {
            ScrubOutcome::Aead(bytes) => {
                counters.scrub_aead_verified += 1;
                counters.scrub_blocks_scanned += 1;
                counters.scrub_bytes_scanned += bytes;
            }
            ScrubOutcome::Frame(bytes) => {
                counters.scrub_frame_verified += 1;
                counters.scrub_blocks_scanned += 1;
                counters.scrub_bytes_scanned += bytes;
            }
            ScrubOutcome::ReadableOnly(bytes) => {
                counters.scrub_readability_only += 1;
                counters.scrub_blocks_scanned += 1;
                counters.scrub_bytes_scanned += bytes;
            }
            ScrubOutcome::Failed(evidence) => {
                counters.scrub_blocks_scanned += 1;
                // Suspect verification: online, a failure re-verifies
                // under a validated pin after re-resolving the mapping —
                // a moved mapping or an in-flight patch clears.
                let confirmed = if opts.mode == FsckMode::Online {
                    reverify_scrub_failure(ctx, &crypto, m, block_size).await
                } else {
                    true
                };
                if confirmed {
                    counters.scrub_failures += 1;
                    findings.push(FsckFinding {
                        class: "C7".to_string(),
                        object: format!("ino {} block {} ({})", m.ino, m.block_idx, m.mapping),
                        evidence,
                        identity: Some(FindingId::C7Scrub {
                            ino: m.ino,
                            block_idx: m.block_idx,
                            mapping: m.mapping.clone(),
                        }),
                    });
                } else {
                    counters.suspects_cleared += 1;
                }
            }
            ScrubOutcome::Skipped => {}
        }
        in_batch += 1;
        if in_batch >= SCRUB_BATCH {
            throttle(opts, t0.elapsed()).await;
            t0 = std::time::Instant::now();
            in_batch = 0;
        }
    }
}

enum ScrubOutcome {
    Aead(u64),
    Frame(u64),
    ReadableOnly(u64),
    Failed(String),
    Skipped,
}

/// Read the stored image for a mapping and verify what its stored form
/// makes verifiable (KD-17).
async fn verify_stored_block(
    ctx: &FsckCtx,
    crypto: &crate::crypto_compress::CryptoCompressState,
    m: &MappingRef,
    block_size: usize,
) -> ScrubOutcome {
    let br = &ctx.router.backend_router;
    let clean = clean_key(&m.mapping);
    // Decoration: `bk:rel:len` carries the EXACT stored image geometry.
    let decorated = {
        let rest = match m.mapping.find("://") {
            Some(p) => &m.mapping[p + 3..],
            None => m.mapping.as_str(),
        };
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() == 3 {
            match (parts[1].parse::<u64>(), parts[2].parse::<usize>()) {
                (Ok(rel), Ok(len)) => Some((rel, len)),
                _ => None,
            }
        } else {
            None
        }
    };
    let (base_be, base_off) = match br.parse_block_key(&clean) {
        Ok(x) => x,
        Err(_) => return ScrubOutcome::Skipped, // C2 lost owns unparseable mappings
    };
    let Some((_vol, _alloc)) = canonical_backend(ctx, &base_be) else {
        return ScrubOutcome::Skipped;
    };
    let (read_off, read_len, exact_len) = match decorated {
        Some((rel, len)) => (base_off + rel, len.div_ceil(4096) * 4096, Some(len)),
        None => {
            // Undecorated whole-block mapping: transformed images carry
            // a self-delimiting frame and may exceed `block_size`
            // (encrypt envelope + frame — the FIND-RW4-A headroom), so
            // the window is the reader-side stored-image bound.
            let window = if crypto.is_passthrough() {
                block_size
            } else {
                crypto
                    .max_stored_image_len(block_size)
                    .div_ceil(4096)
                    .saturating_mul(4096)
            };
            (base_off, window, None)
        }
    };
    if let Some(hook) = SCRUB_READ_FAULT_HOOK.read().clone() {
        if hook(read_off) {
            return ScrubOutcome::Failed(
                "device read error: injected fault (test hook)".to_string(),
            );
        }
    }
    let (_, dev) = match br.get_backend(&base_be) {
        Ok(x) => x,
        Err(_) => {
            // Backend registered but unhealthy — reads refuse; a scrub of
            // a disabled volume reports the read status honestly.
            return ScrubOutcome::Failed("backend offline for scrub read".to_string());
        }
    };
    let image = match dev.read_block(read_off, read_len).await {
        Ok(b) => b,
        Err(e) => return ScrubOutcome::Failed(format!("device read error: {e}")),
    };
    if let Some(len) = exact_len {
        if image.len() < len {
            return ScrubOutcome::Failed(format!(
                "short device read: {} of {len} bytes",
                image.len()
            ));
        }
    }
    let image = match exact_len {
        Some(len) if image.len() > len => image.slice(0..len),
        _ => image,
    };
    let bytes = image.len() as u64;
    if crypto.is_passthrough() {
        // No stored checksum exists: read success is the honest verdict
        // (`scrub_readability_only` — the OQ-B gap, stated not faked).
        return ScrubOutcome::ReadableOnly(bytes);
    }
    match crypto.process_read(&image) {
        Ok(_) => {
            if crypto.encrypt_mode != crate::crypto_compress::EncryptMode::None {
                ScrubOutcome::Aead(bytes)
            } else {
                ScrubOutcome::Frame(bytes)
            }
        }
        Err(e) => {
            if crypto.encrypt_mode != crate::crypto_compress::EncryptMode::None {
                ScrubOutcome::Failed(format!("AEAD verification failed: {e}"))
            } else {
                ScrubOutcome::Failed(format!("transform frame undecodable: {e}"))
            }
        }
    }
}

/// Online scrub-failure re-verification: the mapping must still be
/// referenced verbatim, and the re-read must fail again under a
/// VALIDATED pin (an in-flight patch — unstable incarnation — or a
/// racing free clears the suspect instead of reporting a torn read).
async fn reverify_scrub_failure(
    ctx: &FsckCtx,
    crypto: &crate::crypto_compress::CryptoCompressState,
    m: &MappingRef,
    block_size: usize,
) -> bool {
    // Re-resolve the ino's CURRENT layout.
    let (vol_idx, local) = ctx.meta.route_ino(m.ino);
    let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
        return false;
    };
    let Ok(Some(bytes)) = kv.getxattr(local, "layout").await else {
        return false; // layout gone: mapping superseded
    };
    let layout: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{") {
        serde_json::from_slice(&bytes).ok()
    } else {
        bincode::deserialize(&bytes).ok()
    };
    let Some(layout) = layout else { return false };
    let mut still_mapped = layout
        .block_map
        .as_ref()
        .is_some_and(|bm| bm.get(&m.block_idx).is_some_and(|v| v == &m.mapping));
    if !still_mapped {
        // Indirect/kvmap maps: re-read through the census decoder for
        // this single layout.
        let mut probe = probe_census();
        let tree_entries = kvmap_entries_for(ctx, kv, local, &layout).await;
        census_layout(ctx, m.ino, &layout, block_size, tree_entries, &mut probe).await;
        still_mapped = probe.mappings.iter().any(|p| p.mapping == m.mapping);
    }
    if !still_mapped {
        return false;
    }
    let clean = clean_key(&m.mapping);
    match ctx.router.backend_router.pin_block_validated(&clean) {
        crate::block_allocator::PinOutcome::Pinned => {
            let outcome = verify_stored_block(ctx, crypto, m, block_size).await;
            let _ = ctx.router.backend_router.free_block(&clean).await; // unpin
            matches!(outcome, ScrubOutcome::Failed(_))
        }
        crate::block_allocator::PinOutcome::PinnedUnstable => {
            let _ = ctx.router.backend_router.free_block(&clean).await; // unpin
            false // patch mid-flight: torn read, not corruption
        }
        crate::block_allocator::PinOutcome::Refused => false, // freed under us
    }
}

// ---------------------------------------------------------------------------
// Throttle + metrics
// ---------------------------------------------------------------------------

async fn throttle(opts: &FsckOptions, elapsed: Duration) {
    throttle_sleep(opts.throttle_pct, elapsed).await;
}

/// The KD-3 duty-cycle sleep by percentage — the `FsckOptions`-free form
/// the spawned per-tree C1 walks use.
async fn throttle_sleep(throttle_pct: u32, elapsed: Duration) {
    if let Some(delay) = crate::jobs::job_throttle_sleep(elapsed, throttle_pct) {
        squeezefs_ipc::sqz_time::sleep(delay).await;
    }
}

fn publish_metrics(c: &FsckCounters) {
    use crate::fuse_client::METRICS;
    let m = &*METRICS;
    m.fsck_inodes_scanned
        .fetch_add(c.inodes_scanned, Ordering::Relaxed);
    m.fsck_nodes_walked
        .fetch_add(c.nodes_walked, Ordering::Relaxed);
    m.fsck_dentry_refs_indexed
        .fetch_add(c.dentry_refs_indexed, Ordering::Relaxed);
    m.fsck_current_era_exempted
        .fetch_add(c.current_era_exempted, Ordering::Relaxed);
    m.fsck_unreferenced_intent_exempted
        .fetch_add(c.unreferenced_intent_exempted, Ordering::Relaxed);
    m.fsck_nlink_names_counted
        .fetch_add(c.nlink_names_counted, Ordering::Relaxed);
    m.fsck_nlink_mismatch_high
        .fetch_add(c.nlink_mismatch_high, Ordering::Relaxed);
    m.fsck_nlink_mismatch_low
        .fetch_add(c.nlink_mismatch_low, Ordering::Relaxed);
    m.fsck_nlink_zero_named
        .fetch_add(c.nlink_zero_named, Ordering::Relaxed);
    m.fsck_dangling_dentries
        .fetch_add(c.dangling_dentries, Ordering::Relaxed);
    m.fsck_nlink_transient_cleared
        .fetch_add(c.nlink_transient_cleared, Ordering::Relaxed);
    // KD-PV-16's coverage gauge is the LAST pass's answer, not a running
    // total: "did this pass reach the whole plane?" has no cumulative
    // reading. The two scoping counters and the two admission counters
    // accumulate like every other engagement gauge.
    if c.inode_plane_volumes_covered > 0 {
        m.fsck_inode_plane_volumes_covered
            .store(c.inode_plane_volumes_covered, Ordering::Relaxed);
    }
    m.fsck_inode_plane_foreign_scoped
        .fetch_add(c.inode_plane_foreign_scoped, Ordering::Relaxed);
    m.fsck_inode_plane_foreign_slot_scoped
        .fetch_add(c.inode_plane_foreign_slot_scoped, Ordering::Relaxed);
    m.fsck_inode_plane_window_scoped
        .fetch_add(c.inode_plane_window_scoped, Ordering::Relaxed);
    m.fsck_inode_plane_foreign_dentry_scoped
        .fetch_add(c.inode_plane_foreign_dentry_scoped, Ordering::Relaxed);
    m.fsck_c1_projection_slots_scoped
        .fetch_add(c.c1_projection_slots_scoped, Ordering::Relaxed);
    if c.inode_plane_slots_covered > 0 {
        m.fsck_inode_plane_slots_covered
            .store(c.inode_plane_slots_covered, Ordering::Relaxed);
    }
    m.fsck_inode_plane_cross_owner_declined
        .fetch_add(c.inode_plane_cross_owner_declined, Ordering::Relaxed);
    m.fsck_inode_plane_proposals_admitted
        .fetch_add(c.inode_plane_proposals_admitted, Ordering::Relaxed);
    m.fsck_inode_plane_proposals_stripped
        .fetch_add(c.inode_plane_proposals_stripped, Ordering::Relaxed);
    m.fsck_blocks_checked
        .fetch_add(c.blocks_checked, Ordering::Relaxed);
    m.fsck_refcounts_checked
        .fetch_add(c.refcounts_checked, Ordering::Relaxed);
    m.fsck_suspects.fetch_add(c.suspects, Ordering::Relaxed);
    m.fsck_suspects_cleared
        .fetch_add(c.suspects_cleared, Ordering::Relaxed);
    m.fsck_epoch_exempted
        .fetch_add(c.epoch_exempted, Ordering::Relaxed);
    m.fsck_inflight_exempted
        .fetch_add(c.inflight_exempted, Ordering::Relaxed);
    m.fsck_mover_ledger_exempted
        .fetch_add(c.mover_ledger_exempted, Ordering::Relaxed);
    m.fsck_pack_ledger_exempted
        .fetch_add(c.pack_ledger_exempted, Ordering::Relaxed);
    m.fsck_foreign_lane_exempted
        .fetch_add(c.foreign_lane_exempted, Ordering::Relaxed);
    m.fsck_alloc_bitmap_leak_candidates
        .fetch_add(c.alloc_bitmap_leak_candidates, Ordering::Relaxed);
    m.fsck_alloc_bitmap_tracked_exempted
        .fetch_add(c.alloc_bitmap_tracked_exempted, Ordering::Relaxed);
    m.fsck_map_orphan_records
        .fetch_add(c.map_orphan_records, Ordering::Relaxed);
    m.fsck_map_empty_heads
        .fetch_add(c.map_empty_heads, Ordering::Relaxed);
    m.fsck_map_run_foreign_shadows
        .fetch_add(c.map_run_foreign_shadows, Ordering::Relaxed);
    m.fsck_crossing_exempted
        .fetch_add(c.crossing_exempted, Ordering::Relaxed);
    m.fsck_tenant_overlap_findings
        .fetch_add(c.tenant_overlap_findings, Ordering::Relaxed);
    m.fsck_shared_index_drift
        .fetch_add(c.shared_index_drift, Ordering::Relaxed);
    m.fsck_stripe_findings
        .fetch_add(c.stripe_findings, Ordering::Relaxed);
    m.fsck_slot_custody_conflicts
        .fetch_add(c.slot_custody_conflicts, Ordering::Relaxed);
    m.fsck_unrecovered_appenders
        .fetch_add(c.unrecovered_appenders, Ordering::Relaxed);
    m.fsck_findings.fetch_add(c.findings, Ordering::Relaxed);
    m.fsck_scan_secs.store(c.scan_secs, Ordering::Relaxed);
    m.scrub_blocks_scanned
        .fetch_add(c.scrub_blocks_scanned, Ordering::Relaxed);
    m.scrub_bytes_scanned
        .fetch_add(c.scrub_bytes_scanned, Ordering::Relaxed);
    m.scrub_aead_verified
        .fetch_add(c.scrub_aead_verified, Ordering::Relaxed);
    m.scrub_frame_verified
        .fetch_add(c.scrub_frame_verified, Ordering::Relaxed);
    m.scrub_readability_only
        .fetch_add(c.scrub_readability_only, Ordering::Relaxed);
    m.scrub_failures
        .fetch_add(c.scrub_failures, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// PR VL6b — §5.6a repair: per-class actions, dry-run default,
// quarantine-first, verify-before-repair
// ---------------------------------------------------------------------------

/// Repair invocation options. **Dry-run is the default** (`apply =
/// false`): the planner emits per-finding actions and mutates NOTHING.
#[derive(Clone, Debug, Default)]
pub struct RepairOptions {
    /// Execute the plan (`--repair --apply`). Requires coordinator /
    /// D0-guarded authority — the CLI enforces the posture; the engine
    /// enforces per-object leases.
    pub apply: bool,
    /// Quarantine home override (`--quarantine-dir`). Default:
    /// `<first staging dir>/quarantine/`; REQUIRED on cache-less
    /// filesystems when any action needs quarantine.
    pub quarantine_dir: Option<PathBuf>,
    /// **KD-PV-8**: a multi-owner plane is armed on the repairing mount,
    /// so the destructive/dangerous trio is REPORT-ONLY (§5.9.3). Set
    /// from the caller's OWN truth — the mount derives it from its live
    /// `OwnerMap`; the offline harness is always `false` because the
    /// whole-set pass requires every owner unmounted. Never read from a
    /// report: a payload-declared posture would be a repair trusting the
    /// thing it is supposed to be protected from.
    pub multi_owner: bool,
}

/// One planned / applied / refused action (§5.6a table verbs).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RepairAction {
    pub class: String,
    pub object: String,
    /// The table verb: `rebuild-in-place`, `quarantine-report-only`,
    /// `free-leaked-block`, `repair-allocator`, `quarantine-mapping`,
    /// `recount-and-set-refcount`, `quarantine-then-discard-custody`,
    /// `quarantine-staging-dir`, `recompute-accounting`.
    pub action: String,
    pub detail: String,
}

/// §10 repair counters, per run (the process gauges in
/// [`crate::fuse_client::METRICS`] accumulate the same names).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct RepairCounters {
    pub planned: u64,
    pub applied: u64,
    /// Verify-before-repair refusals (stale/healed findings, missing
    /// identity, unreachable authority). Never an error: the state
    /// moved on and the repair honestly declined.
    pub refused: u64,
    /// The SUBSET of [`Self::refused`] declined because a multi-owner
    /// plane is armed and the action is one of the destructive trio
    /// (§5.9.3): C9's `destroy-unreferenced-inode`, C10's
    /// `lower-nlink-to-counted-names`, C10's `remove-dangling-dentry`.
    /// **Expected NONZERO** on a multi-owner online pass with such
    /// findings and **0 on the offline whole-set pass** — the inverted
    /// reading is the point.
    pub refused_multi_owner: u64,
    pub quarantined_records: u64,
    pub quarantined_blocks: u64,
    pub quarantined_bytes: u64,
    /// Applied repairs per class (`"C1"`..`"C10"`).
    pub per_class: std::collections::BTreeMap<String, u64>,
}

/// The structured repair report (embedded in [`FsckReport::repair`]).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RepairReport {
    pub schema: u32,
    pub dry_run: bool,
    pub planned: Vec<RepairAction>,
    pub applied: Vec<RepairAction>,
    /// Refusals with their reasons in `detail`.
    pub refused: Vec<RepairAction>,
    /// The per-run quarantine directory, when anything was quarantined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantine_dir: Option<String>,
    pub counters: RepairCounters,
}

/// Kill-9-window test hook: invoked with `"<class>:<object>"` AFTER the
/// action's quarantine copies are durable and BEFORE its commit
/// mutation. Returning `true` aborts the run right there (the injected
/// crash) — quarantine exists, the mutation never happened, the finding
/// is re-detected by the next run (the §5.6a convergence law under
/// test). `None` in production.
static REPAIR_ABORT_HOOK: parking_lot::RwLock<Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>> =
    parking_lot::RwLock::new(None);

pub fn set_repair_abort_hook(hook: Arc<dyn Fn(&str) -> bool + Send + Sync>) {
    *REPAIR_ABORT_HOOK.write() = Some(hook);
}

pub fn clear_repair_abort_hook() {
    *REPAIR_ABORT_HOOK.write() = None;
}

fn fire_repair_abort_hook(what: &str) -> Result<()> {
    let hook = REPAIR_ABORT_HOOK.read().clone();
    if let Some(h) = hook {
        if h(what) {
            return Err(SqueezefsError::InvalidOperation(format!(
                "fsck repair aborted by test hook at {what} (injected kill-9 window: \
                 quarantine durable, commit never ran — re-run converges)"
            )));
        }
    }
    Ok(())
}

/// The per-run quarantine directory + JSON manifest (§5.6a: "nothing is
/// destroyed without a copy"). Lazily created on the first quarantined
/// byte; every entry append rewrites + fdatasyncs `manifest.json`
/// BEFORE the action's commit mutation runs, so a crash between
/// quarantine and commit always leaves an auditable copy.
struct Quarantine {
    home: Option<PathBuf>,
    run_dir: Option<PathBuf>,
    entries: Vec<serde_json::Value>,
    seq: u32,
}

impl Quarantine {
    fn new(ctx: &FsckCtx, opts: &RepairOptions) -> Self {
        let home = opts
            .quarantine_dir
            .clone()
            .or_else(|| ctx.staging_dirs.first().map(|d| d.join("quarantine")));
        Quarantine {
            home,
            run_dir: None,
            entries: Vec::new(),
            seq: 0,
        }
    }

    async fn run_dir(&mut self) -> Result<PathBuf> {
        if let Some(d) = &self.run_dir {
            return Ok(d.clone());
        }
        let home = self.home.clone().ok_or_else(|| {
            SqueezefsError::InvalidOperation(
                "repair needs a quarantine home and this filesystem is cache-less: \
                 pass --quarantine-dir (§5.6a — nothing is destroyed without a copy)"
                    .to_string(),
            )
        })?;
        let run_id = format!(
            "{:x}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            std::process::id()
        );
        let dir = home.join(run_id);
        {
            let dir = dir.clone();
            squeezefs_ipc::sqz_blocking::run_blocking(move || std::fs::create_dir_all(&dir)).await
        }
        .map_err(|e| {
            SqueezefsError::Io(std::io::Error::new(
                e.kind(),
                format!("creating quarantine dir {}: {e}", dir.display()),
            ))
        })?;
        self.run_dir = Some(dir.clone());
        Ok(dir)
    }

    /// Copy `parts` (suffix → bytes) into the run dir, fsync each, append
    /// the manifest entry, rewrite + fsync the manifest. Returns total
    /// bytes copied.
    async fn put(
        &mut self,
        class: &str,
        object: &str,
        action: &str,
        note: &str,
        parts: &[(&str, &[u8])],
    ) -> Result<u64> {
        let dir = self.run_dir().await?;
        self.seq += 1;
        let seq = self.seq;
        let mut files = Vec::new();
        let mut total = 0u64;
        for (i, (suffix, bytes)) in parts.iter().enumerate() {
            let name = format!("{seq:04}_{class}_{i}_{suffix}.bin");
            let path = dir.join(&name);
            crate::uring_fs::write_all(&path, bytes.to_vec()).await?;
            crate::uring_fs::fdatasync(&path).await?;
            total += bytes.len() as u64;
            files.push(serde_json::json!({ "name": name, "bytes": bytes.len() }));
        }
        self.entries.push(serde_json::json!({
            "class": class,
            "object": object,
            "action": action,
            "note": note,
            "files": files,
        }));
        self.write_manifest().await?;
        Ok(total)
    }

    async fn write_manifest(&mut self) -> Result<()> {
        let dir = self.run_dir().await?;
        let manifest = serde_json::json!({
            "schema": 1u32,
            "created_unix": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "entries": self.entries,
        });
        let path = dir.join("manifest.json");
        crate::uring_fs::write_all(
            &path,
            serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
        )
        .await?;
        crate::uring_fs::fdatasync(&path).await
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

/// The §5.6a table verb a finding plans to (dry run and apply share the
/// planner — the plan IS what apply executes).
/// **The destructive/dangerous trio** (§5.9.1's table, verified against
/// the repair arms themselves): the actions whose FALSE POSITIVE
/// destroys data or reachability, as opposed to leaking it.
///
/// * `C9Unreferenced` → `destroy-unreferenced-inode` — a live file is
///   destroyed. The most destructive act in the fsck surface.
/// * `C10NlinkTooHigh` → `lower-nlink-to-counted-names` — a named inode
///   becomes reclaimable.
/// * `C10DanglingDentry` → `remove-dangling-dentry` — a live name
///   disappears.
///
/// Everything else either leaks (the safe raises: an over-count delays
/// reclaim, and the next pass corrects it) or is already report-only
/// (C8) or quarantine-first over an object the finding fully identifies.
fn destroys_under_multi_owner(id: &FindingId) -> bool {
    matches!(
        id,
        FindingId::C9Unreferenced { .. }
            | FindingId::C10NlinkTooHigh { .. }
            | FindingId::C10DanglingDentry { .. }
    )
}

/// The raw C1 repair's deletion gate (design-symmetric-metadata §7.1's
/// corollary, review round 3 Issue 25): `Ok(description)` when the key is
/// a KNOWN slot-tree kind in a shape it never takes — garbage by the
/// codec's own law, repairable in place — and `Err(reason)` for every
/// other refused key: a kind byte this binary does not know may be a
/// LATER binary's record (a new slot-tree kind ships under a new incompat
/// bit, but the repair never bets the volume on the bit having been
/// honoured), and a key too short to carry a kind cannot be NAMED. Those
/// are reported, never deleted — the C1 torn posture.
fn raw_key_repair_gate(key_hex: &str) -> std::result::Result<String, String> {
    use crate::meta_backend::kv::record::{classify_refused_key, RawKeyDefect};
    let Some(key) = unhex(key_hex) else {
        return Err("undecodable key identity".to_string());
    };
    match classify_refused_key(&key) {
        None => Err("the key now decodes under the forest codec (not this class)".to_string()),
        Some(RawKeyDefect::MalformedKnownKind { kind, want, got }) => Ok(format!(
            "kind {kind} is a slot-tree kind of this binary and its key is {got} bytes where the \
             codec wants {want}"
        )),
        Some(RawKeyDefect::SlotOutOfNamespace { kind, slot }) => Ok(format!(
            "kind {kind} is a slot-tree kind of this binary and its routing ino names slot \
             {slot}, above the u16 slot namespace no encoder ever frames into"
        )),
        Some(RawKeyDefect::UnknownKind { kind }) => Err(format!(
            "kind byte {kind:#04x} is not a slot-tree kind this binary knows — a later \
             binary's record or garbage; REPORT-ONLY (design-symmetric-metadata §7.1: a new \
             slot-tree kind ships under a new incompat bit, and the raw repair never deletes \
             what it cannot name)"
        )),
        Some(RawKeyDefect::Truncated { got }) => Err(format!(
            "a {got}-byte key carries no kind byte — no kind of this binary can name it; \
             REPORT-ONLY (the raw repair deletes only what it can name)"
        )),
    }
}

fn planned_action(id: &FindingId) -> (&'static str, String) {
    match id {
        FindingId::C1Torn { .. } => (
            "quarantine-report-only",
            "torn node: no replicas exist — quarantine identity + report, never \
             fabrication (data loss made visible and bounded)"
                .to_string(),
        ),
        FindingId::C1Semantic { .. } => (
            "rebuild-in-place",
            "checksum-valid structural damage: quarantine the record bytes, then drop \
             the schema-violating record via an ordinary journaled CoW leaf re-emit"
                .to_string(),
        ),
        FindingId::C1RawKey { key_hex, .. } => match raw_key_repair_gate(key_hex) {
            Ok(defect) => (
                "rebuild-in-place",
                format!(
                    "checksum-valid slot-tree record under a key the forest codec refuses \
                     ({defect}): quarantine the raw bytes, then drop the record from its slot \
                     tree (the kind-routed reads already skip it; no live key resolves to it)"
                ),
            ),
            Err(why) => ("report-only", why),
        },
        FindingId::C2Leaked { vol, offset } => (
            "free-leaked-block",
            format!(
                "free {vol}:{offset} via begin_free → purge → punch → finish_free \
                 (bytes quarantined first)"
            ),
        ),
        FindingId::C2Lost {
            unrepairable_shape, ..
        } => {
            if *unrepairable_shape {
                (
                    "quarantine-mapping",
                    "out-of-range/unresolvable mapping: replace with an explicit \
                     `damaged:` marker (reads EIO) — a hole is never silently fabricated"
                        .to_string(),
                )
            } else {
                (
                    "verify-content-then-repair-allocator-or-quarantine",
                    "verify the block content first; verifiable ⇒ repair the allocator \
                     (the data was fine, the accounting was wrong); unverifiable ⇒ \
                     quarantine the mapping (`damaged:` marker, reads EIO)"
                        .to_string(),
                )
            }
        }
        FindingId::C3Refcount { vol, offset } => (
            "recount-and-set-refcount",
            format!(
                "recount {vol}:{offset}'s references under every referencing ino's \
                 DLM lease (ascending) and set the refcount to the counted value"
            ),
        ),
        FindingId::C4Orphan { key, .. } => (
            "quarantine-then-discard-custody",
            format!(
                "quarantine a verbatim copy of the staged record + payload for '{key}', \
                 then discard via the recovery discard law (magic retire)"
            ),
        ),
        FindingId::C5Staging { dir } => (
            "quarantine-staging-dir",
            format!(
                "move {}'s stale-generation marker + staged segment files aside into \
                 quarantine (the discard-on-mismatch law with a copy retained)",
                dir.display()
            ),
        ),
        FindingId::C6Drift { vol } => (
            "recompute-accounting",
            format!("recompute {vol}'s derived used/free accounting from the tracked census"),
        ),
        FindingId::C8DurableRefDrift { vol, offset } => (
            "restate-durable-refs",
            format!(
                "restate {vol}:{offset}'s durable reference records from the layout                  walk's verified census (the layouts are the justification; the ledger                  is the index)"
            ),
        ),
        FindingId::C9Unreferenced { ino } => (
            "destroy-unreferenced-inode",
            format!(
                "quarantine ino {ino}'s inode record + every xattr record (the layout that \
                 names its blocks) and enumerate the block keys in the manifest, then free \
                 those blocks through the ordinary terminal-free law (durable references \
                 released, discards queued on the reclaimer) and destroy the record in ONE \
                 journaled transaction. Block CONTENTS are not copied — an inode's data is \
                 unbounded, so the manifest states which blocks were reclaimed rather than \
                 pretending to keep them"
            ),
        ),
        FindingId::C10NlinkTooHigh { ino } => (
            "lower-nlink-to-counted-names",
            format!(
                "quarantine ino {ino}'s inode record, then set nlink to the DEDUPED \
                 distinct names counted under its exclusive lease. Lowering a count is \
                 the one C10 repair that could make a named inode reclaimable if the \
                 count were wrong, so it runs only when a third independent dentry pass \
                 agrees, the record is unchanged, no cross-volume plan is open, and the \
                 counted names are ≥ 1 (0 names is C9's object, never this one)"
            ),
        ),
        FindingId::C10NlinkTooLow { ino } => (
            "raise-nlink-to-counted-names",
            format!(
                "quarantine ino {ino}'s inode record, then RAISE nlink to the deduped \
                 distinct names counted under its exclusive lease. Raising is the safe \
                 direction: an over-count delays reclaim (a leak), while dropping a name \
                 to match a low count would be data loss and is never the repair"
            ),
        ),
        FindingId::C10ZeroNlinkNamed { ino } => (
            "raise-nlink-to-counted-names",
            format!(
                "quarantine ino {ino}'s inode record, then raise nlink from 0 to the \
                 deduped distinct names that resolve to it — making the inode live again \
                 is what the names already say, and it re-arms the live-nlink skip that \
                 keeps reclaim off it. Refused for a directory (its count is 2 + \
                 subdirectories, which this class does not compute)"
            ),
        ),
        FindingId::C10DanglingDentry { child_ino, .. } => (
            "remove-dangling-dentry",
            format!(
                "quarantine the dentry record (key AND value) and then delete it: the \
                 name resolves to nothing because ino {child_ino} has no inode record, so \
                 removal is the only possible repair (there is nothing to re-point it \
                 at). Refused when the named child was a DIRECTORY — that removal must \
                 also decide the parent's directory nlink, which this class does not \
                 verify"
            ),
        ),
        FindingId::C7Scrub { mapping, .. } => (
            "quarantine-mapping",
            format!(
                "replace '{mapping}' with an explicit `damaged:` marker (reads EIO); \
                 the physical block stays in place for forensics"
            ),
        ),
        FindingId::C11OrphanMapRecords { vol, ino, records } => (
            "report-only",
            format!(
                "{records} orphan block-map record(s) on vol{vol} for owner ino {ino}: \
                 REPORT-ONLY by design (design-kvmap-block-map-tree A3 — a false \
                 quarantine would hole a live crossing); the next crossing's A1 residue \
                 sweep is the reclaim path"
            ),
        ),
        FindingId::C11EmptyKvmapHead { vol, ino } => (
            "report-only",
            format!(
                "kvmap head on vol{vol} ino {ino} declares nonzero size with ZERO tree \
                 records: REPORT-ONLY (the C8 posture) — restating the map would \
                 fabricate data; the operator adjudicates"
            ),
        ),
        FindingId::C11RunForeignShadow {
            vol,
            ino,
            run_start,
            idx,
        } => (
            "report-only",
            format!(
                "cross-volume point at index {idx} inside the run at [{run_start}, …) on \
                 vol{vol} ino {ino}: REPORT-ONLY (design §12) — the point supersedes by \
                 the §2 read law and the next full publish re-canonicalizes"
            ),
        ),
        FindingId::C12Overlap {
            vol,
            offset,
            ino_a,
            block_idx_a,
            ino_b,
            block_idx_b,
        } => (
            "report-only",
            format!(
                "tenant windows of ino {ino_a} block {block_idx_a} and ino {ino_b} block \
                 {block_idx_b} intersect at different offsets on {vol}:{offset}: REPORT-ONLY \
                 (design-small-file-packing §5.9, the C8 posture) — at least one tenant is \
                 wrong and nothing on the volume says which; quarantining both would destroy \
                 the right one"
            ),
        ),
        FindingId::C12Overrun {
            vol,
            offset,
            ino,
            block_idx,
            mapping,
        } => (
            "report-only",
            format!(
                "the window of ino {ino} block {block_idx} ('{mapping}') on {vol}:{offset} \
                 breaks the size-carrying mapping's law: REPORT-ONLY — the base block IS \
                 referenced and only the window is unreadable (reads refuse EIO); the right \
                 repair is the owner's, not a guess"
            ),
        ),
        FindingId::C13OrphanImageExtent {
            vol,
            appender,
            extent,
        } => (
            "return-orphan-image-extent",
            format!(
                "return heap extent {extent} of vol{vol} to the bitmap: appender {appender} \
                 frees it in its OWN ring (a `free` gated on its tail, the SMO retirement \
                 shape) and the cadence's ReturnExtents clears the bit and rewrites the grant \
                 record — nothing routes to the image, so nothing is quarantined"
            ),
        ),
        FindingId::C16SharedIndexDrift {
            vol_tag,
            block_idx,
            owner_ino,
            ..
        } => (
            "report-only",
            format!(
                "shared-index drift on block {block_idx} of data volume {vol_tag:#x} (ino \
                 {owner_ino}) is REPORT-ONLY: the SHARED flag and the index are two durable \
                 homes of one fact, and the block's next release re-derives the truth from \
                 both (design-symmetric-metadata §5.4.4's crash windows)"
            ),
        ),
        FindingId::C17StripeInconsistency {
            dir, stripe, shape, ..
        } => (
            "report-only",
            format!(
                "stripe inconsistency `{shape}` on directory {dir} / stripe {stripe} is \
                 REPORT-ONLY (the C8 posture): the map and the stripes are the durable \
                 homes of one directory's names, and restating one from the other would \
                 erase the evidence of which side lied (design-symmetric-metadata §5.6.5)"
            ),
        ),

        FindingId::C14SlotCustodyConflict {
            vol,
            slot,
            appender_a,
            appender_b,
        } => (
            "report-only",
            format!(
                "slot {slot} of vol{vol} attested by appenders {appender_a} and {appender_b} at \
                 once is REPORT-ONLY: which page is dead is the operator's attestation — \
                 `squeezefs appender clear <sqmeta-uri> <id>` writes the death record and marks \
                 the page Recovering, and the next mount recovers it (design-symmetric-metadata \
                 §5.8.5 / §6.2)"
            ),
        ),
        FindingId::C15UnrecoveredAppender {
            vol,
            appender,
            window_entries,
            ..
        } => (
            "recover-dead-appender",
            format!(
                "run the §5.9 recovery of appender {appender} on vol{vol} ({window_entries} \
                 acked entries in its ring window): preempt its registrant, replay the window \
                 into its slot trees, flush, record every leaf's tail, unlease its slots in \
                 tree 0, return its grant, mark the page Recovered — online on the volume's \
                 manager; an offline run names the mount path, which recovers before serving"
            ),
        ),
    }
}

/// The ino's CURRENT layout through the census's own extraction (the
/// identity-precise fresh read every mapping-class re-check runs):
/// every referenced mapping, resolvable or not. Empty when the ino has
/// no decodable layout.
async fn probe_layout_mappings(ctx: &FsckCtx, ino: u64) -> Vec<MappingRef> {
    let (vol_idx, local) = ctx.meta.route_ino(ino);
    let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
        return Vec::new();
    };
    let Ok(Some(bytes)) = kv.getxattr(local, "layout").await else {
        return Vec::new();
    };
    let layout: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{") {
        serde_json::from_slice(&bytes).ok()
    } else {
        bincode::deserialize(&bytes).ok()
    };
    let Some(layout) = layout else {
        return Vec::new();
    };
    let block_size = ctx
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let mut probe = probe_census();
    let tree_entries = kvmap_entries_for(ctx, kv, local, &layout).await;
    census_layout(ctx, ino, &layout, block_size, tree_entries, &mut probe).await;
    let mut out = probe.mappings;
    out.append(&mut probe.unresolvable);
    out
}

/// Is the ino's CURRENT layout still carrying `mapping` (verbatim,
/// non-quarantined) at `block_idx`? The identity-precise
/// verify-before-repair re-check for the mapping classes.
async fn current_mapping_present(ctx: &FsckCtx, ino: u64, block_idx: u32, mapping: &str) -> bool {
    probe_layout_mappings(ctx, ino)
        .await
        .iter()
        .any(|m| m.ino == ino && m.block_idx == block_idx && m.mapping == mapping && !m.damaged)
}

/// C12's fresh read: each target's CURRENT non-quarantined mapping at its
/// block index, all read under the inos' exclusive 4a leases held
/// TOGETHER (online) — one consistent cut, acquired in the DLM's one
/// canonical order (ascending volume index, `lock_many` inside each: a
/// pair sharing a stripe is one acquisition, never a self-deadlock). A
/// layout publish takes the same lease, so nothing can move either
/// layout between the two reads. Offline nothing is in flight and no
/// lease is taken.
async fn current_tenant_mappings(
    ctx: &FsckCtx,
    online: bool,
    targets: &[(u64, u32)],
) -> Vec<Option<MappingRef>> {
    use crate::meta_backend::dlm::LockMode;
    let mut by_vol: std::collections::BTreeMap<usize, Vec<(u64, LockMode)>> =
        std::collections::BTreeMap::new();
    if online {
        for &(ino, _) in targets {
            let (vol_idx, local) = ctx.meta.route_ino(ino);
            by_vol
                .entry(vol_idx)
                .or_default()
                .push((local, LockMode::Exclusive));
        }
    }
    let mut guards = Vec::new();
    for (vol_idx, inos) in &by_vol {
        if let Some(kv) = ctx.meta.volumes.get(*vol_idx) {
            guards.extend(kv.dlm().lock_many(inos, &[]).await);
        }
    }
    let mut out = Vec::with_capacity(targets.len());
    for &(ino, block_idx) in targets {
        out.push(
            probe_layout_mappings(ctx, ino)
                .await
                .into_iter()
                .find(|m| m.block_idx == block_idx && !m.damaged),
        );
    }
    drop(guards);
    out
}

/// [`current_tenant_mappings`] decoded to C12's terms: the block each
/// target names NOW as `(vol, offset, incarnation)` and its window. `None`
/// when the target no longer maps that index or its mapping is no longer
/// a decodable window (an overrun is C12Overrun's, never an overlap's).
async fn current_tenant_windows(
    ctx: &FsckCtx,
    online: bool,
    targets: &[(u64, u32)],
) -> Vec<Option<((String, u64, u64), TenantWindow)>> {
    let chunk = crate::block_allocator::CHUNK_SIZE;
    current_tenant_mappings(ctx, online, targets)
        .await
        .into_iter()
        .map(|m| {
            let m = m?;
            let WindowClass::Window { off, end } = tenant_window_class(&m.mapping, chunk) else {
                return None;
            };
            let inc = crate::routing::block_key_incarnation(&m.mapping)
                .unwrap_or(crate::routing::INCARNATION_NONE);
            Some((
                (m.vol, m.offset, inc),
                TenantWindow {
                    ino: m.ino,
                    block_idx: m.block_idx,
                    mapping: m.mapping,
                    off,
                    end,
                },
            ))
        })
        .collect()
}

/// Flip a mapping to its §5.6a `damaged:` quarantine marker — one
/// tx-atomic layout commit under the merge discipline (the 4a lease is
/// taken inside the meta transaction). **Supersession-safe**: the flip
/// rides [`crate::routing::BlockMapOp::MergeExpected`] — it lands only
/// where the captured mapping is STILL current, so a foreground rewrite
/// or a mover publish racing the repair is never overwritten (`false` =
/// superseded, the caller refuses the action). The displaced original
/// key is deliberately NOT freed: for C7 the physical block is
/// preserved for forensics; for C2-lost there is nothing allocated to
/// free.
#[doc(hidden)] // visible for the B4c-i belt pin (tests/overlay_overwrite_tests.rs)
pub async fn flip_mapping_damaged(
    ctx: &FsckCtx,
    ino: u64,
    block_idx: u32,
    mapping: &str,
) -> Result<bool> {
    let token = ctx.router.dlm.get_fencing_token_ino(ino);
    let damaged = format!("{}{mapping}", crate::routing::DAMAGED_MAPPING_PREFIX);
    let entries = [(block_idx, mapping.to_string(), damaged)];
    let displaced = ctx
        .router
        .merge_block_mappings(
            ino,
            crate::routing::BlockMapOp::MergeExpected(&entries),
            0,
            crate::routing::LayoutFlip::KeepLayout,
            token,
        )
        .await?;
    Ok(displaced.iter().any(|d| d == mapping))
}

/// Best-effort raw copy of a mapping's stored image for quarantine
/// (`None` when the device window cannot be read — recorded honestly in
/// the manifest instead of blocking the isolation).
async fn read_stored_image_best_effort(ctx: &FsckCtx, mapping: &str) -> Option<bytes::Bytes> {
    let block_size = ctx
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let clean = clean_key(mapping);
    let (be_id, offset) = ctx.router.backend_router.parse_block_key(&clean).ok()?;
    let (_, alloc_dev) = canonical_backend(ctx, &be_id)?;
    let _ = alloc_dev; // canonicalization proves the backend exists
    let (_, dev) = ctx.router.backend_router.get_backend(&be_id).ok()?;
    let crypto = ctx.router.get_crypto();
    let window = if crypto.is_passthrough() {
        block_size
    } else {
        crypto
            .max_stored_image_len(block_size)
            .div_ceil(4096)
            .saturating_mul(4096)
    };
    dev.read_block(offset, window).await.ok()
}

/// Run the §5.6a repair over a VERIFIED report's findings. Dry-run by
/// default (`opts.apply = false`): plans + returns, mutating nothing.
/// Apply mode re-verifies every finding is STILL current before acting
/// (verify-before-repair — a healed/stale finding is a refused repair,
/// counted), quarantines before every discard, and executes each action
/// as one tx-atomic commit / copy-then-retire step so kill-9 anywhere
/// leaves a consistent filesystem and a re-run converges.
pub async fn repair(
    ctx: &FsckCtx,
    report: &FsckReport,
    opts: &RepairOptions,
) -> Result<RepairReport> {
    let mut out = RepairReport {
        schema: FSCK_REPORT_SCHEMA,
        dry_run: !opts.apply,
        planned: Vec::new(),
        applied: Vec::new(),
        refused: Vec::new(),
        quarantine_dir: None,
        counters: RepairCounters::default(),
    };

    // ---- Plan (shared by dry run and apply) ----
    let mut actionable: Vec<(&FsckFinding, &FindingId)> = Vec::new();
    for f in &report.findings {
        match &f.identity {
            Some(id) => {
                let (verb, detail) = planned_action(id);
                out.planned.push(RepairAction {
                    class: f.class.clone(),
                    object: f.object.clone(),
                    action: verb.to_string(),
                    detail,
                });
                actionable.push((f, id));
            }
            None => {
                out.refused.push(RepairAction {
                    class: f.class.clone(),
                    object: f.object.clone(),
                    action: "refused".to_string(),
                    detail: "finding carries no structured identity (older-binary report) \
                             — re-run detection with this binary"
                        .to_string(),
                });
            }
        }
    }
    out.counters.planned = out.planned.len() as u64;
    out.counters.refused = out.refused.len() as u64;

    if !opts.apply {
        publish_repair_metrics(&out.counters);
        return Ok(out);
    }

    // ---- Apply ----
    let online = report.mode == "online";
    let block_size = ctx
        .router
        .block_size
        .load(std::sync::atomic::Ordering::Relaxed) as usize;
    let crypto = ctx.router.get_crypto().clone();
    let vols = volume_allocators(ctx);
    let alloc_of = |vol: &str| vols.iter().find(|v| v.id == vol).map(|v| v.alloc.clone());
    let mut quarantine = Quarantine::new(ctx, opts);

    // One fresh census for the allocator-class verifications (C2/C3),
    // walked ONCE — the repair-time ground truth.
    let needs_census = actionable.iter().any(|(_, id)| {
        matches!(
            id,
            FindingId::C2Leaked { .. } | FindingId::C2Lost { .. } | FindingId::C3Refcount { .. }
        )
    });
    let mut fresh = if needs_census {
        let mut scratch = FsckCounters::default();
        Some(walk_census(ctx, &FsckOptions::offline(), &mut scratch).await?)
    } else {
        None
    };
    // The inode plane's repair-time ground truth: ONE dentry pass (repair
    // is refused under `--shards`, so this is always the whole set) —
    // C9's "is it named" and C10's deduped name identities from the same
    // walk. `None` = the pass could not complete, which REFUSES every
    // inode-plane action: destroying an inode whose name might exist, or
    // lowering a count against a partial name set, is exactly what must
    // not happen.
    let inode_plane_inos: std::collections::HashSet<u64> = actionable
        .iter()
        .filter_map(|(_, id)| match id {
            FindingId::C10NlinkTooHigh { ino }
            | FindingId::C10NlinkTooLow { ino }
            | FindingId::C10ZeroNlinkNamed { ino } => Some(*ino),
            _ => None,
        })
        .collect();
    let repair_refs = if !inode_plane_inos.is_empty()
        || actionable
            .iter()
            .any(|(_, id)| matches!(id, FindingId::C9Unreferenced { .. }))
    {
        build_referenced_inos(
            ctx.meta.clone(),
            None,
            100,
            Arc::new(AtomicBool::new(false)),
            Some(&inode_plane_inos),
        )
        .await
        .0
    } else {
        None
    };
    // C10's in-flight exemption at repair time too: a plan in flight is
    // never repaired around.
    let repair_intents = if inode_plane_inos.is_empty()
        && !actionable.iter().any(|(_, id)| {
            matches!(
                id,
                FindingId::C10DanglingDentry { .. } | FindingId::C9Unreferenced { .. }
            )
        }) {
        std::collections::HashSet::new()
    } else {
        open_intent_inos(ctx).await
    };
    // **The inode plane first, C9 before C10, then everything else.**
    //
    // A C9 destroy deletes a REFERENCER: every block-class finding naming
    // its blocks becomes stale and resolves as a refused (verified-healed)
    // repair, instead of paying quarantine work — or re-allocating blocks —
    // on an inode that is about to be destroyed.
    //
    // C10 follows for the mirror-image reason: the census SKIPS
    // `nlink == 0` records, so a zero-count-with-a-name inode's blocks were
    // walked as allocated-unreferenced (C2-leaked). Raising the count
    // re-attaches them, and freeing them first would destroy the data the
    // raise restores — so the raise must precede the block plane, and the
    // block plane's census is re-walked once the inode plane has moved
    // (`census_stale` below). C9 and C10 never claim the same inode (C9's
    // object is named by nobody, every C10 count object is named by
    // somebody), so their relative order costs neither of them anything.
    // Stable, so every other class keeps its report order.
    actionable.sort_by_key(|(_, id)| match id {
        FindingId::C9Unreferenced { .. } => 0u8,
        FindingId::C10NlinkTooHigh { .. }
        | FindingId::C10NlinkTooLow { .. }
        | FindingId::C10ZeroNlinkNamed { .. }
        | FindingId::C10DanglingDentry { .. } => 1,
        _ => 2,
    });
    // Set by an applied inode-plane raise: the next block-class
    // verification re-walks the census rather than trusting one taken
    // before the inode plane moved.
    let mut census_stale = false;

    let refuse = |out: &mut RepairReport, f: &FsckFinding, why: String| {
        out.refused.push(RepairAction {
            class: f.class.clone(),
            object: f.object.clone(),
            action: "refused".to_string(),
            detail: why,
        });
        out.counters.refused += 1;
    };
    let class_idx = |class: &str| -> Option<usize> {
        class
            .strip_prefix('C')
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| (1..=10).contains(n))
            .map(|n| n - 1)
    };
    let apply_ok = |out: &mut RepairReport, f: &FsckFinding, verb: &str, detail: String| {
        out.applied.push(RepairAction {
            class: f.class.clone(),
            object: f.object.clone(),
            action: verb.to_string(),
            detail,
        });
        out.counters.applied += 1;
        *out.counters.per_class.entry(f.class.clone()).or_insert(0) += 1;
        if let Some(i) = class_idx(&f.class) {
            crate::fuse_client::METRICS.fsck_repair_class[i]
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    };

    for (f, id) in actionable {
        let what = format!("{}:{}", f.class, f.object);
        // ---- KD-PV-8 / §5.9.3: the repair-CONSEQUENCE split ----
        //
        // Under an armed multi-owner plane the three repairs whose FALSE
        // POSITIVE destroys something — C9's `destroy-unreferenced-inode`
        // (a live file), C10's `lower-nlink-to-counted-names` (the code's
        // own text: "the one C10 repair that could make a named inode
        // reclaimable if the count were wrong") and C10's
        // `remove-dangling-dentry` (a live name) — are REPORT-ONLY, on
        // the C8 precedent (detect always, repair never automatic).
        //
        // The safe raises (C10-low, C10-zero-named) stay online: their
        // false-positive source is an OVERCOUNTED reference set, the
        // direction a monotone-behind projection errs in, and their FP
        // consequence is a leak the next pass corrects. The split is by
        // consequence, not by leak-vs-loss direction — those two axes
        // point OPPOSITE ways here, which is the mistake rev 1 made.
        //
        // Why report-only rather than trusting the §5.9.2 freeze: the
        // freeze argument is sound but rests on the M1 pre-check being
        // TOTAL, and that pre-check is introduced by the same program.
        // The finding is visible either way and the offline pass costs a
        // maintenance window, so the trade is not worth taking.
        if opts.multi_owner && destroys_under_multi_owner(id) {
            let (verb, _) = planned_action(id);
            out.refused.push(RepairAction {
                class: f.class.clone(),
                object: f.object.clone(),
                action: "refused".to_string(),
                detail: format!(
                    "'{verb}' is REPORT-ONLY while a multi-owner plane is armed: a false \
                     positive here destroys an inode, a name, or a link count that still \
                     covers a live path, and this node's view of a peer-owned volume is a \
                     monotone-behind projection whose only error direction produces exactly \
                     this class. The finding stands — run the OFFLINE whole-set pass \
                     (`squeezefs fsck --repair --apply <sqmeta-uri>` with every owner \
                     unmounted) to apply it (design-per-volume-claim-admission §5.9.3, \
                     KD-PV-8)"
                ),
            });
            out.counters.refused += 1;
            out.counters.refused_multi_owner += 1;
            continue;
        }
        // An applied inode-plane raise re-attached blocks the census walked
        // as unreferenced, so the block classes must not verify against it:
        // re-walk ONCE, here, where the ordering above guarantees the inode
        // plane is already done.
        if census_stale
            && matches!(
                id,
                FindingId::C2Leaked { .. }
                    | FindingId::C2Lost { .. }
                    | FindingId::C3Refcount { .. }
            )
        {
            let mut scratch = FsckCounters::default();
            fresh = Some(walk_census(ctx, &FsckOptions::offline(), &mut scratch).await?);
            census_stale = false;
        }
        match id {
            // ------------------------------------ C8 durable-ref drift
            //
            // Repair is REFUSED, deliberately, and the refusal is the
            // honest answer rather than a gap (§5.6a "no fabrication where
            // redundancy does not exist"). Restating the ledger from the
            // layout walk is a whole-volume mutation whose safe form needs
            // every referencing ino's lease held across the restate, and a
            // C8 finding means the accounting invariant ALREADY broke — the
            // operator must see the divergence and its cause before a tool
            // overwrites the evidence with the walk's opinion. The layouts
            // are intact either way (they are the justification; the ledger
            // is only its index), and mounting with
            // `SQUEEZEFS_BLOCK_REFS_VERIFY=1` re-proves the state after any
            // manual remedy.
            FindingId::C8DurableRefDrift { vol, offset } => {
                refuse(
                    &mut out,
                    f,
                    format!(
                        "durable block-reference drift at {vol}:{offset} is reported, \
                         never auto-repaired: the ledger and the layouts diverged, and \
                         restating one from the other would erase the evidence of why. \
                         The layouts remain authoritative"
                    ),
                );
                continue;
            }
            // ------------------------------------ C11 map-plane (kvmap)
            //
            // REPORT-ONLY, deliberately (design-kvmap-block-map-tree §3
            // fsck + A3, the C8 posture): a false-positive quarantine of
            // "orphan" records would hole a LIVE crossing's staged map —
            // silent wrong data, the exact failure the tree exists to
            // prevent — and an empty head has nothing safe to restate
            // (fabricating a map is never a repair). Orphan records are
            // reclaimed by the next crossing's A1 residue sweep.
            FindingId::C11OrphanMapRecords { vol, ino, records } => {
                refuse(
                    &mut out,
                    f,
                    format!(
                        "{records} orphan block-map record(s) on vol{vol} for owner ino \
                         {ino} are REPORTED, never auto-repaired (design A3: a false \
                         quarantine would hole a live crossing) — REPORT-ONLY; the next \
                         crossing's A1 residue sweep reclaims them"
                    ),
                );
                continue;
            }
            FindingId::C11EmptyKvmapHead { vol, ino } => {
                refuse(
                    &mut out,
                    f,
                    format!(
                        "the fully-empty kvmap head on vol{vol} ino {ino} is REPORTED, \
                         never auto-repaired — REPORT-ONLY (design A3): restating the \
                         map would fabricate data where redundancy does not exist; the \
                         operator adjudicates"
                    ),
                );
                continue;
            }
            FindingId::C11RunForeignShadow {
                vol,
                ino,
                run_start,
                idx,
            } => {
                refuse(
                    &mut out,
                    f,
                    format!(
                        "the cross-volume point at index {idx} inside the run at \
                         [{run_start}, …) on vol{vol} ino {ino} is REPORTED, never \
                         auto-repaired — REPORT-ONLY (design §12): the point supersedes \
                         by the read law and the next full publish re-canonicalizes"
                    ),
                );
                continue;
            }
            // ------------------------------------ C12 tenant ranges (packing)
            //
            // REFUSED, deliberately — the C8 posture (design-small-file-
            // packing §5.9): two tenants overlapping at different `off`
            // means at least one is wrong and nothing on the volume says
            // which, so quarantining both would destroy the one that is
            // right; an unreadable window's base block IS referenced, and
            // guessing a window would fabricate a mapping. The layouts
            // stay exactly as found for the owner to adjudicate.
            FindingId::C12Overlap {
                vol,
                offset,
                ino_a,
                block_idx_a,
                ino_b,
                block_idx_b,
            } => {
                refuse(
                    &mut out,
                    f,
                    format!(
                        "the overlapping tenant windows of ino {ino_a} block {block_idx_a} \
                         and ino {ino_b} block {block_idx_b} on {vol}:{offset} are REPORTED, \
                         never auto-repaired — REPORT-ONLY: at least one is wrong and nothing \
                         on the volume says which; both layouts stay as found"
                    ),
                );
                continue;
            }
            FindingId::C12Overrun {
                vol,
                offset,
                ino,
                block_idx,
                mapping,
            } => {
                refuse(
                    &mut out,
                    f,
                    format!(
                        "the window of ino {ino} block {block_idx} ('{mapping}') on \
                         {vol}:{offset} is REPORTED, never auto-repaired — REPORT-ONLY: the \
                         base block is referenced and only the window is unreadable; \
                         restating it would fabricate a mapping"
                    ),
                );
                continue;
            }
            // ------------------------------------ C13 orphan image extent
            //
            // C16 is report-only by design (§5.8.5): the SHARED flag and
            // the index entry are two durable homes of one fact; the
            // block's next release consults both.
            FindingId::C16SharedIndexDrift {
                vol_tag, block_idx, ..
            } => {
                refuse(
                    &mut out,
                    f,
                    format!(
                        "shared-index drift on block {block_idx} of data volume {vol_tag:#x} \
                         is reported, never auto-repaired: the block's terminal free \
                         re-derives the truth from the flag and the index together"
                    ),
                );
                continue;
            }
            // C17 is report-only by design (§5.6.5): the stripe map and
            // the stripes' entries are the durable homes of one directory.
            FindingId::C17StripeInconsistency { dir, shape, .. } => {
                refuse(
                    &mut out,
                    f,
                    format!(
                        "stripe inconsistency `{shape}` on directory {dir} is reported, \
                         never auto-repaired (design-symmetric-metadata §5.6.5 — the C8 \
                         posture)"
                    ),
                );
                continue;
            }
            // Verify-before-repair is the backend's: `c13_return_orphan`
            // re-runs the census under the volume's SMO + mint
            // serialization and acts only on an extent still claimed and
            // still unreached — `false` is the healed / stale-report
            // refusal. Nothing is quarantined: no route reaches the image,
            // so there is nothing a reader could lose.
            FindingId::C13OrphanImageExtent {
                vol,
                appender,
                extent,
            } => {
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    refuse(&mut out, f, format!("meta volume {vol} is not mounted"));
                    continue;
                };
                match kv.c13_return_orphan(*appender, *extent).await {
                    Ok(true) => {
                        crate::fuse_client::METRICS
                            .fsck_repair_class_c13
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        apply_ok(
                            &mut out,
                            f,
                            "return-orphan-image-extent",
                            format!(
                                "extent {extent} freed in appender {appender}'s ring; the \
                                 cadence's ReturnExtents clears the bit and rewrites the \
                                 grant record"
                            ),
                        );
                    }
                    Ok(false) => refuse(
                        &mut out,
                        f,
                        format!(
                            "extent {extent} is no longer a claimed orphan of appender \
                             {appender} (healed, returned, or reached by a root since the \
                             report) — re-run detection"
                        ),
                    ),
                    Err(e) => refuse(
                        &mut out,
                        f,
                        format!("returning extent {extent} of appender {appender} failed: {e}"),
                    ),
                }
                continue;
            }
            // ------------------------------------ C14 slot custody conflict
            //
            // Report-only by design (§5.8.5): the census cannot know
            // which of two Live attestations is the dead one — that is
            // the operator's attestation, `squeezefs appender clear`.
            FindingId::C14SlotCustodyConflict {
                vol,
                slot,
                appender_a,
                appender_b,
            } => {
                refuse(
                    &mut out,
                    f,
                    format!(
                        "slot {slot} of vol{vol} is attested by appenders {appender_a} and \
                         {appender_b} at once: reported, never auto-repaired — which page is \
                         dead is the operator's attestation (`squeezefs appender clear \
                         <sqmeta-uri> <id>`); the mount refuses until it is given"
                    ),
                );
                continue;
            }
            // ------------------------------------ C15 un-recovered appender
            //
            // Verify-before-repair is the recovery's own: it re-reads the
            // ledger and the page under the volume's SMO serialization
            // and acts on a Live/Recovering page of a ledgered identity
            // only. Online = this mount is the volume's manager (the
            // recovery is the manager's act — a non-manager answers no
            // region); offline runs name the mount path, which recovers
            // before serving.
            FindingId::C15UnrecoveredAppender { vol, appender, .. } => {
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    refuse(&mut out, f, format!("meta volume {vol} is not mounted"));
                    continue;
                };
                if !online {
                    refuse(
                        &mut out,
                        f,
                        format!(
                            "appender {appender} of vol{vol} is recovered by the volume's \
                             MANAGER: the next writer mount recovers it before serving (the \
                             mount-path gate), or the live manager's ledger poll does — an \
                             offline probe holds no ring and no lease to recover under"
                        ),
                    );
                    continue;
                }
                let Some(vol0) = ctx.meta.volumes.first() else {
                    refuse(&mut out, f, "the set has no volume 0".to_string());
                    continue;
                };
                let Ok(ordinal) = u16::try_from(*vol) else {
                    refuse(
                        &mut out,
                        f,
                        format!("meta volume ordinal {vol} is out of range"),
                    );
                    continue;
                };
                match kv.recover_dead_appenders(vol0, ordinal).await {
                    Ok(rep) if rep.recovered.iter().any(|r| r.appender_id == *appender) => {
                        crate::fuse_client::METRICS
                            .fsck_repair_class_c15
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        apply_ok(
                            &mut out,
                            f,
                            "recover-dead-appender",
                            format!(
                                "appender {appender}'s window replayed into its slot trees, \
                                 its slots unleased in tree 0, its grant returned, the page \
                                 Recovered"
                            ),
                        );
                    }
                    Ok(rep) => refuse(
                        &mut out,
                        f,
                        format!(
                            "appender {appender} of vol{vol} was not recovered by this mount \
                             ({} recovered, {} deferred): it is no longer a ledgered Live / \
                             Recovering page, this mount is not the volume's manager, or its \
                             recovery is deferred to the successor — re-run detection",
                            rep.recovered.len(),
                            rep.deferred
                        ),
                    ),
                    Err(e) => refuse(
                        &mut out,
                        f,
                        format!("recovering appender {appender} of vol{vol} failed: {e}"),
                    ),
                }
                continue;
            }
            // ------------------------------------ C9 unreferenced inode
            //
            // The one repair that DESTROYS an inode, so its verification
            // is the strictest: still live, still prior-era, and still
            // named by nothing in a freshly walked referenced set.
            //
            // Step order is chosen for its crash windows (each leaves a
            // state the next run converges from, and none leaves a state
            // no class names):
            //
            // 1. quarantine the record + every xattr (the layout IS the
            //    map to the blocks) and enumerate the block keys;
            // 2. `delete_file` — durable references released, blocks
            //    freed through the ordinary terminal-free law (reclaim
            //    queue, non-reallocatable until reclaimed, fence latch
            //    observed), staged custody purged, tiers coherent;
            // 3. destroy the record + its xattrs in ONE transaction.
            //
            // Interrupted between 2 and 3 the inode is still `nlink >= 1`
            // and unnamed, so the next run re-detects it as C9 and both
            // remaining steps are idempotent (an already-free key frees
            // as a no-op; a ref release is a `Delete`). While interrupted,
            // the block classes may ALSO name its blocks (C2-lost, and on
            // a bit-8 volume C8 drift) — which is why C9 runs first: after
            // its destroy those findings verify as healed and refuse.
            FindingId::C9Unreferenced { ino } => {
                // Issue 10: a plan in flight is never repaired around —
                // the roll-forward names this record.
                if repair_intents.contains(ino) {
                    refuse(
                        &mut out,
                        f,
                        "an open cross-volume plan names this inode: its name is the \
                         plan's roll-forward to land, never C9's to destroy"
                            .to_string(),
                    );
                    continue;
                }
                let (vol_idx, local) = ctx.meta.route_ino(*ino);
                let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
                    refuse(&mut out, f, "volume index no longer exists".to_string());
                    continue;
                };
                // Verify under the ino's exclusive 4a lease; the raw
                // per-volume read (the routed `getattr` would self-deadlock
                // against it — the C4 lesson).
                let verified = {
                    let _lease = if online {
                        Some(kv.dlm().lock_inode_exclusive(local).await)
                    } else {
                        None
                    };
                    match kv.read_inode_value_routed(local).await {
                        Ok(Some(v)) if v.nlink > 0 => Ok(v),
                        Ok(Some(_)) => Err("the inode is nlink 0 now (an ordinary reclaim \
                                            owns it — C9 never claims that shape)"
                            .to_string()),
                        Ok(None) => Err("the inode record is gone (already reclaimed)".to_string()),
                        Err(e) => Err(format!("inode record unreadable at verify: {e}")),
                    }
                };
                let value = match verified {
                    Ok(v) => v,
                    Err(why) => {
                        refuse(&mut out, f, why);
                        continue;
                    }
                };
                if !kv.minted_in_prior_era(local) {
                    refuse(
                        &mut out,
                        f,
                        "the ino belongs to THIS mount's writer era now (it cannot be \
                         prior-era residue) — nothing is destroyed on a re-used report"
                            .to_string(),
                    );
                    continue;
                }
                match &repair_refs {
                    Some(pass) if pass.refs.contains(*ino) => {
                        refuse(
                            &mut out,
                            f,
                            "a dentry names this inode now (reconnected since the scan)"
                                .to_string(),
                        );
                        continue;
                    }
                    Some(_) => {}
                    None => {
                        refuse(
                            &mut out,
                            f,
                            "the referenced-ino pass could not complete: repair refuses \
                             rather than destroy an inode whose name may exist"
                                .to_string(),
                        );
                        continue;
                    }
                }
                // Quarantine: the inode record verbatim + every xattr
                // record (key AND value — `layout` is the only map to the
                // blocks this repair reclaims).
                use crate::meta_backend::kv::record::{inode_key, xattr_key, HASH56_MAX};
                let ikey = inode_key(local);
                let Ok(Some(record)) = kv
                    .lookup_kind(crate::meta_backend::kv::record::TREE_INODES, &ikey)
                    .await
                else {
                    refuse(
                        &mut out,
                        f,
                        "the inode record vanished between verify and quarantine".to_string(),
                    );
                    continue;
                };
                let mut parts_owned: Vec<(String, Vec<u8>)> =
                    vec![("inode_record".to_string(), record.to_vec())];
                let mut xattr_records = 0u64;
                {
                    let end = xattr_key(local, HASH56_MAX, u8::MAX);
                    let mut cursor: Vec<u8> = xattr_key(local, 0, 0).to_vec();
                    while let Ok(page) = kv
                        .range_kind(
                            crate::meta_backend::kv::record::TREE_XATTRS,
                            &cursor,
                            &end,
                            SCAN_PAGE,
                        )
                        .await
                    {
                        let Some((last, _)) = page.last() else { break };
                        cursor = crate::meta_backend::kv::node::key_successor(last);
                        for (k, v) in &page {
                            parts_owned.push((format!("xattr_{xattr_records}_key"), k.to_vec()));
                            parts_owned.push((format!("xattr_{xattr_records}_value"), v.to_vec()));
                            xattr_records += 1;
                        }
                    }
                }
                let mappings = layout_mappings_of(ctx, *ino).await;
                let keys: Vec<String> = mappings.iter().map(|m| m.mapping.clone()).collect();
                let note = format!(
                    "unreferenced inode {ino}: record + {xattr_records} xattr record(s), \
                     nlink {} size {} B; blocks reclaimed by this repair: [{}] — block \
                     CONTENTS are deliberately NOT copied (an inode's data is unbounded), \
                     so this manifest states what was reclaimed instead of pretending to \
                     keep it",
                    value.nlink,
                    value.size,
                    keys.join(", ")
                );
                let parts: Vec<(&str, &[u8])> = parts_owned
                    .iter()
                    .map(|(n, b)| (n.as_str(), b.as_slice()))
                    .collect();
                let bytes = quarantine
                    .put(
                        &f.class,
                        &f.object,
                        "destroy-unreferenced-inode",
                        &note,
                        &parts,
                    )
                    .await?;
                out.counters.quarantined_records += 1 + xattr_records;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                // Free the data FIRST: the layout is the only map to these
                // blocks, and the destroy reaps it. A failure here LEAVES
                // the record (the finding survives, the next run retries) —
                // never a silently stranded block population.
                match ctx.router.delete_file(&crate::keys::inode_path(*ino)).await {
                    Ok(()) => {}
                    // No layout at all: the crashed-cross-volume-create
                    // shape (an inode that never got a byte written).
                    Err(e) if is_not_found(&e) => {}
                    Err(e) => {
                        refuse(
                            &mut out,
                            f,
                            format!(
                                "data teardown failed ({e}) — the inode record is \
                                 deliberately LEFT so the next run retries; nothing was \
                                 destroyed"
                            ),
                        );
                        continue;
                    }
                }
                ctx.meta.destroy_unreferenced_inodes(&[*ino]).await?;
                apply_ok(
                    &mut out,
                    f,
                    "destroy-unreferenced-inode",
                    format!(
                        "ino {ino} destroyed with its {xattr_records} xattr record(s) in one \
                         journaled transaction; {} block(s) reclaimed through the terminal-free \
                         law (record + xattrs quarantined first)",
                        keys.len()
                    ),
                );
            }
            // ------------------------------ C10 nlink vs counted names
            //
            // ONE verification ladder for all three inode-plane arms; the
            // DIRECTION only chooses the delta's sign, so a future arm
            // cannot acquire a weaker check by accident. Order:
            //
            // 1. no OPEN cross-volume plan names this ino (a plan in flight
            //    is exactly the window where a count and its names
            //    legitimately disagree);
            // 2. under the ino's exclusive 4a lease: the record still
            //    exists, and it is not a directory (a directory's count is
            //    2 + subdirectories — this class does not compute that, so
            //    it refuses rather than writing a number it cannot verify);
            // 3. the repair-time dentry pass — a THIRD independent count,
            //    deduped by `(global parent, name)` — still disagrees with
            //    the record, in the direction the finding claims;
            // 4. quarantine the record bytes, then ONE journaled
            //    `routed_nlink_adjust` under the SAME lease.
            //
            // A crash anywhere leaves either the old count (re-detected) or
            // the new one (healed): both converge, and the quarantine copy
            // exists either way.
            FindingId::C10NlinkTooHigh { ino }
            | FindingId::C10NlinkTooLow { ino }
            | FindingId::C10ZeroNlinkNamed { ino } => {
                if repair_intents.contains(ino) {
                    refuse(
                        &mut out,
                        f,
                        "an open cross-volume plan names this inode: its count and its \
                         names are allowed to disagree until the plan retires"
                            .to_string(),
                    );
                    continue;
                }
                let Some(pass) = repair_refs.as_ref() else {
                    refuse(
                        &mut out,
                        f,
                        "the dentry pass could not complete: repair refuses rather than \
                         write a link count derived from a partial name set"
                            .to_string(),
                    );
                    continue;
                };
                let names = pass.collected(*ino).len() as u32;
                let (vol_idx, local) = ctx.meta.route_ino(*ino);
                let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
                    refuse(&mut out, f, "volume index no longer exists".to_string());
                    continue;
                };
                // ONE lease across verify AND mutate: the adjust holds it
                // (never re-acquires it — that would self-deadlock the
                // non-reentrant stripe lock), so nothing can move the count
                // between the two.
                let lease: Arc<[crate::meta_backend::dlm::DlmGuard]> = if online {
                    Arc::from(vec![kv.dlm().lock_inode_exclusive(local).await])
                } else {
                    Arc::from(Vec::new())
                };
                let value = match kv.read_inode_value_routed(local).await {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        refuse(
                            &mut out,
                            f,
                            "the inode record is gone (already reclaimed) — the name, if \
                             one remains, is the dangling-dentry arm's object"
                                .to_string(),
                        );
                        continue;
                    }
                    Err(e) => {
                        refuse(&mut out, f, format!("inode record unreadable: {e}"));
                        continue;
                    }
                };
                if value.mode & libc::S_IFMT == libc::S_IFDIR {
                    refuse(
                        &mut out,
                        f,
                        "this inode is a DIRECTORY: its nlink counts `.` and every \
                         child's `..`, so the number of dentry names is not the count to \
                         write. Reported, never guessed at (parent/subdirectory \
                         accounting is outside this class)"
                            .to_string(),
                    );
                    continue;
                }
                if names == 0 {
                    refuse(
                        &mut out,
                        f,
                        "no dentry names this inode now: an unreferenced inode is class \
                         C9's object (destroy + reclaim), and lowering a count to 0 here \
                         would strand it instead"
                            .to_string(),
                    );
                    continue;
                }
                if value.nlink == names {
                    refuse(
                        &mut out,
                        f,
                        format!(
                            "nlink {names} already matches the {names} deduped name(s) \
                             (healed since the scan, or the scan's record count included \
                             a slot migration's in-flight duplicate)"
                        ),
                    );
                    continue;
                }
                let lowering = value.nlink > names;
                if lowering != matches!(id, FindingId::C10NlinkTooHigh { .. }) {
                    refuse(
                        &mut out,
                        f,
                        format!(
                            "the disagreement reversed direction since the scan (nlink \
                             {} vs {names} name(s)): re-run detection rather than apply \
                             a stale verdict",
                            value.nlink
                        ),
                    );
                    continue;
                }
                let ikey = crate::meta_backend::kv::record::inode_key(local);
                let Ok(Some(record)) = kv
                    .lookup_kind(crate::meta_backend::kv::record::TREE_INODES, &ikey)
                    .await
                else {
                    refuse(
                        &mut out,
                        f,
                        "the inode record vanished between verify and quarantine".to_string(),
                    );
                    continue;
                };
                let note = format!(
                    "ino {ino} inode record before the link-count repair: nlink {} → \
                     {names} (the deduped distinct names: {})",
                    value.nlink,
                    pass.collected(*ino)
                        .iter()
                        .map(|n| format!("{}/{}", n.parent, String::from_utf8_lossy(&n.name)))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let bytes = quarantine
                    .put(
                        &f.class,
                        &f.object,
                        if lowering {
                            "lower-nlink-to-counted-names"
                        } else {
                            "raise-nlink-to-counted-names"
                        },
                        &note,
                        &[("inode_record", &record)],
                    )
                    .await?;
                out.counters.quarantined_records += 1;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                let delta = i64::from(names) - i64::from(value.nlink);
                match kv.routed_nlink_adjust(local, delta, false, lease).await {
                    Ok(post) => {
                        // A raise re-attaches this inode's blocks to the
                        // live census the block classes verify against.
                        if !lowering {
                            census_stale = true;
                        }
                        apply_ok(
                            &mut out,
                            f,
                            if lowering {
                                "lower-nlink-to-counted-names"
                            } else {
                                "raise-nlink-to-counted-names"
                            },
                            format!(
                                "ino {ino} nlink {} → {} to match its {names} deduped \
                                 name(s), in one journaled transaction under the ino's \
                                 exclusive lease (record quarantined first)",
                                value.nlink, post.nlink
                            ),
                        );
                    }
                    Err(e) => refuse(
                        &mut out,
                        f,
                        format!(
                            "the link-count commit failed ({e}) — nothing was changed and \
                             the finding survives for the next run"
                        ),
                    ),
                }
            }
            // ------------------------------------ C10 dangling dentry
            //
            // The name resolves to nothing, so removal is the ONLY
            // possible repair — there is nothing to re-point it at and
            // nothing to fabricate. Verified under the PARENT's exclusive
            // 4a lease (the dentry lock class): the record still exists
            // VERBATIM at its exact key, still names this ino, and the ino
            // still has no record.
            FindingId::C10DanglingDentry {
                vol,
                key_hex,
                child_ino,
            } => {
                if repair_intents.contains(child_ino) {
                    refuse(
                        &mut out,
                        f,
                        "an open cross-volume plan names this inode: its record may still \
                         be minted by the plan's roll-forward"
                            .to_string(),
                    );
                    continue;
                }
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    refuse(&mut out, f, "volume index no longer exists".to_string());
                    continue;
                };
                let Some(key) = unhex(key_hex) else {
                    refuse(&mut out, f, "undecodable dentry key identity".to_string());
                    continue;
                };
                let Ok((local_parent, _, _)) =
                    crate::meta_backend::kv::record::decode_dentry_key(&key)
                else {
                    refuse(&mut out, f, "undecodable dentry key identity".to_string());
                    continue;
                };
                let _lease = if online {
                    Some(kv.dlm().lock_inode_exclusive(local_parent).await)
                } else {
                    None
                };
                let record = match kv
                    .lookup_kind(crate::meta_backend::kv::record::TREE_DENTRIES, &key)
                    .await
                {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        refuse(
                            &mut out,
                            f,
                            "the dentry record is already gone (removed since the scan)"
                                .to_string(),
                        );
                        continue;
                    }
                    Err(e) => {
                        refuse(&mut out, f, format!("dentry record unreadable: {e}"));
                        continue;
                    }
                };
                let Ok(dentry) = crate::meta_backend::kv::record::DentryValue::decode(&record)
                else {
                    refuse(
                        &mut out,
                        f,
                        "the dentry value no longer decodes (class C1's object)".to_string(),
                    );
                    continue;
                };
                if dentry.child_ino != *child_ino {
                    refuse(
                        &mut out,
                        f,
                        format!(
                            "the name now points at ino {} instead of {child_ino} \
                             (reused since the scan)",
                            dentry.child_ino
                        ),
                    );
                    continue;
                }
                if dentry.file_type == libc::DT_DIR {
                    refuse(
                        &mut out,
                        f,
                        "the name referenced a DIRECTORY: removing it must also decrement \
                         the parent's directory nlink, and this class does not verify a \
                         directory's count — reported for an operator, never guessed at"
                            .to_string(),
                    );
                    continue;
                }
                let (child_vol, child_local) = ctx.meta.route_ino(*child_ino);
                let child_present = match ctx.meta.volumes.get(child_vol) {
                    Some(child_kv) => !matches!(
                        child_kv.read_inode_value_routed(child_local).await,
                        Ok(None)
                    ),
                    None => true,
                };
                if child_present {
                    refuse(
                        &mut out,
                        f,
                        format!(
                            "ino {child_ino} has an inode record again: the name resolves, \
                             so there is nothing dangling to remove"
                        ),
                    );
                    continue;
                }
                let note = format!(
                    "dangling dentry record before removal: '{}' in local parent \
                     {local_parent} on volume {vol}, naming ino {child_ino} (no inode \
                     record); key and value are both copied, so the name can be \
                     reconstructed exactly",
                    String::from_utf8_lossy(&dentry.name)
                );
                let bytes = quarantine
                    .put(
                        &f.class,
                        &f.object,
                        "remove-dangling-dentry",
                        &note,
                        &[("dentry_key", &key), ("dentry_value", &record)],
                    )
                    .await?;
                out.counters.quarantined_records += 1;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                match kv
                    .delete_kind(crate::meta_backend::kv::record::TREE_DENTRIES, &key)
                    .await
                {
                    Ok(()) => apply_ok(
                        &mut out,
                        f,
                        "remove-dangling-dentry",
                        format!(
                            "the name resolving to ino {child_ino} was removed (key and \
                             value quarantined first); the inode it named does not exist, \
                             so no link count changes"
                        ),
                    ),
                    Err(e) => refuse(
                        &mut out,
                        f,
                        format!(
                            "the dentry removal failed ({e}) — the name survives for the \
                             next run"
                        ),
                    ),
                }
            }
            // -------------------------------------------------- C1 torn
            FindingId::C1Torn {
                vol,
                tree,
                slot,
                cursor_hex,
            } => {
                // Verify: the walk still fails from this cursor.
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    refuse(&mut out, f, "volume index no longer exists".to_string());
                    continue;
                };
                let Some(cursor) = unhex(cursor_hex) else {
                    refuse(&mut out, f, "undecodable cursor identity".to_string());
                    continue;
                };
                let unit = match slot {
                    Some(s) => C1Unit::Slot(*s),
                    None => {
                        if !crate::meta_backend::kv::backend::KvMetaBackend::USER_KINDS
                            .contains(tree)
                        {
                            refuse(&mut out, f, "tree no longer exists".to_string());
                            continue;
                        }
                        C1Unit::Kind(*tree)
                    }
                };
                // Same-shaped read as the scan (a max=1 probe can be
                // satisfied by a healthy left sibling and never touch the
                // damaged node — the recheck's own lesson).
                match c1_page(kv, unit, &cursor).await {
                    Ok(_) => {
                        refuse(
                            &mut out,
                            f,
                            "walk now succeeds from the recorded cursor (healed / \
                             transient I/O at scan time)"
                                .to_string(),
                        );
                        continue;
                    }
                    Err(e) => {
                        // The honest action: no replicas exist — record the
                        // identity + error in the quarantine manifest and
                        // REPORT. The finding persists by design (data loss
                        // made visible, never fabricated away).
                        let note = format!(
                            "torn node on vol{vol}/tree{tree} at cursor {cursor_hex}: {e}; \
                             node bytes are unreachable through the validated read path \
                             (checksum-refused) — identity recorded, no mutation performed"
                        );
                        let bytes = quarantine
                            .put(&f.class, &f.object, "quarantine-report-only", &note, &[])
                            .await?;
                        out.counters.quarantined_records += 1;
                        out.counters.quarantined_bytes += bytes;
                        fire_repair_abort_hook(&what)?;
                        apply_ok(&mut out, f, "quarantine-report-only", note);
                    }
                }
            }
            // ------------------------------------------- C1 raw slot key
            FindingId::C1RawKey { vol, slot, key_hex } => {
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    refuse(&mut out, f, "volume index no longer exists".to_string());
                    continue;
                };
                let Some(key) = unhex(key_hex) else {
                    refuse(&mut out, f, "undecodable key identity".to_string());
                    continue;
                };
                // The deletion gate: a known kind in a shape it never
                // takes is dropped; an unknown kind (a later binary's
                // record?) and a key too short to name are REPORTED and
                // left exactly where they are.
                let defect = match raw_key_repair_gate(key_hex) {
                    Ok(defect) => defect,
                    Err(why) => {
                        refuse(&mut out, f, why);
                        continue;
                    }
                };
                let value = match kv.slot_tree_lookup_raw(*slot, &key).await {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        refuse(&mut out, f, "record no longer exists (healed)".to_string());
                        continue;
                    }
                    Err(e) => {
                        refuse(&mut out, f, format!("record unreadable at verify: {e}"));
                        continue;
                    }
                };
                let bytes = quarantine
                    .put(
                        &f.class,
                        &f.object,
                        "rebuild-in-place",
                        "raw slot-tree record bytes (key, value) before the drop",
                        &[("key", &key), ("value", &value)],
                    )
                    .await?;
                out.counters.quarantined_records += 1;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                kv.slot_tree_delete_raw(*slot, &key).await.map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "rebuild-in-place drop of the undecodable slot-tree record failed: {e}"
                    ))
                })?;
                apply_ok(
                    &mut out,
                    f,
                    "rebuild-in-place",
                    format!(
                        "undecodable record ({defect}) dropped from vol{vol}/slot{slot} \
                         (journaled CoW re-emit); bytes quarantined"
                    ),
                );
            }
            // ---------------------------------------------- C1 semantic
            FindingId::C1Semantic { vol, tree, key_hex } => {
                let Some(kv) = ctx.meta.volumes.get(*vol) else {
                    refuse(&mut out, f, "volume index no longer exists".to_string());
                    continue;
                };
                let Some(key) = unhex(key_hex) else {
                    refuse(&mut out, f, "undecodable key identity".to_string());
                    continue;
                };
                if !crate::meta_backend::kv::backend::KvMetaBackend::USER_KINDS.contains(tree) {
                    refuse(&mut out, f, "tree no longer exists".to_string());
                    continue;
                }
                // Verify under the owning ino's lease where one exists.
                let _lease = match (online, owning_ino(*tree, &key)) {
                    (true, Some(local)) => Some(kv.dlm().lock_inode_exclusive(local).await),
                    _ => None,
                };
                let value = match kv.lookup_kind(*tree, &key).await {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        refuse(&mut out, f, "record no longer exists (healed)".to_string());
                        continue;
                    }
                    Err(e) => {
                        refuse(&mut out, f, format!("record unreadable at verify: {e}"));
                        continue;
                    }
                };
                if record_schema_violation(*tree, &key, &value).is_none() {
                    refuse(
                        &mut out,
                        f,
                        "record now satisfies its tree schema (superseded in place)".to_string(),
                    );
                    continue;
                }
                // Quarantine the record bytes, then drop it via the
                // ordinary journaled CoW mutation (the leaf re-emits
                // without the record; the parent pointer swap rides the
                // existing SMO-path machinery).
                let bytes = quarantine
                    .put(
                        &f.class,
                        &f.object,
                        "rebuild-in-place",
                        "record bytes (key, value) before the drop",
                        &[("key", &key), ("value", &value)],
                    )
                    .await?;
                out.counters.quarantined_records += 1;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                kv.delete_kind(*tree, &key).await.map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "rebuild-in-place drop of the schema-violating record failed: {e}"
                    ))
                })?;
                apply_ok(
                    &mut out,
                    f,
                    "rebuild-in-place",
                    format!(
                        "schema-violating record dropped from vol{vol}/tree{tree} \
                         (journaled CoW re-emit); bytes quarantined"
                    ),
                );
            }
            // ------------------------------------------------ C2 leaked
            FindingId::C2Leaked { vol, offset } => {
                let Some(alloc) = alloc_of(vol) else {
                    refuse(&mut out, f, format!("volume '{vol}' no longer registered"));
                    continue;
                };
                let fresh = fresh.as_ref().expect("census walked");
                static EMPTY: once_cell::sync::Lazy<HashMap<u64, u32>> =
                    once_cell::sync::Lazy::new(HashMap::new);
                let refs = fresh.refs.get(vol).unwrap_or(&EMPTY);
                if alloc.refcount(*offset).is_none() {
                    refuse(
                        &mut out,
                        f,
                        "offset no longer tracked (already freed)".to_string(),
                    );
                    continue;
                }
                if refs.contains_key(offset) || alloc.inflight_contains(*offset) {
                    refuse(
                        &mut out,
                        f,
                        "offset is referenced or has a live in-flight owner now \
                         (published since the scan)"
                            .to_string(),
                    );
                    continue;
                }
                // Quarantine the block bytes before the free destroys them.
                let key = ctx.router.backend_router.persist_block_key(vol, *offset);
                let (note, parts): (String, Vec<(&str, &[u8])>);
                let image = read_stored_image_best_effort(ctx, &key).await;
                match &image {
                    Some(img) => {
                        note = "leaked block bytes before the free".to_string();
                        parts = vec![("block", img.as_ref())];
                    }
                    None => {
                        note = "block bytes unreadable at quarantine time — identity \
                                recorded only"
                            .to_string();
                        parts = Vec::new();
                    }
                }
                let bytes = quarantine
                    .put(&f.class, &f.object, "free-leaked-block", &note, &parts)
                    .await?;
                out.counters.quarantined_blocks += 1;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                ctx.router.backend_router.free_block(&key).await?;
                apply_ok(
                    &mut out,
                    f,
                    "free-leaked-block",
                    format!("{vol}:{offset} freed (begin → purge → punch → finish)"),
                );
            }
            // -------------------------------------------------- C2 lost
            FindingId::C2Lost {
                vol,
                offset,
                ino,
                block_idx,
                mapping,
                unrepairable_shape,
            } => {
                if !current_mapping_present(ctx, *ino, *block_idx, mapping).await {
                    refuse(
                        &mut out,
                        f,
                        "the lost mapping is no longer present (truncated / rewritten / \
                         already quarantined)"
                            .to_string(),
                    );
                    continue;
                }
                let repair_allocator = if *unrepairable_shape {
                    false
                } else {
                    let Some(alloc) = alloc_of(vol) else {
                        refuse(&mut out, f, format!("volume '{vol}' no longer registered"));
                        continue;
                    };
                    if alloc.refcount(*offset).is_some() {
                        refuse(
                            &mut out,
                            f,
                            "offset is allocator-tracked now (healed)".to_string(),
                        );
                        continue;
                    }
                    // Verify the content first (the C7 check for this
                    // block's stored form).
                    let probe = MappingRef {
                        ino: *ino,
                        block_idx: *block_idx,
                        mapping: mapping.clone(),
                        vol: vol.clone(),
                        offset: *offset,
                        damaged: false,
                    };
                    !matches!(
                        verify_stored_block(ctx, &crypto, &probe, block_size).await,
                        ScrubOutcome::Failed(_) | ScrubOutcome::Skipped
                    )
                };
                if repair_allocator {
                    let alloc = alloc_of(vol).expect("checked above");
                    let counted = fresh
                        .as_ref()
                        .and_then(|c| c.refs.get(vol).and_then(|m| m.get(offset)).copied())
                        .unwrap_or(1)
                        .max(1);
                    fire_repair_abort_hook(&what)?;
                    alloc.recover_block(*offset / alloc.chunk_size()).await?;
                    alloc.fsck_set_refcount(*offset, counted);
                    // The content is durable on the device: restore fill
                    // stability under a fresh incarnation generation.
                    alloc.publish_block(*offset);
                    apply_ok(
                        &mut out,
                        f,
                        "repair-allocator",
                        format!(
                            "content verified ⇒ {vol}:{offset} re-marked allocated at \
                             refcount {counted} (the data was fine, the accounting \
                             was wrong)"
                        ),
                    );
                } else {
                    // Quarantine the mapping: copy what is readable,
                    // then flip to the explicit damaged marker.
                    let image = read_stored_image_best_effort(ctx, mapping).await;
                    let (note, parts): (String, Vec<(&str, &[u8])>) = match &image {
                        Some(img) => (
                            "stored image window at quarantine time".to_string(),
                            vec![("block", img.as_ref())],
                        ),
                        None => (
                            "stored image unreadable (out-of-range / unresolvable) — \
                             identity recorded only"
                                .to_string(),
                            Vec::new(),
                        ),
                    };
                    let bytes = quarantine
                        .put(&f.class, &f.object, "quarantine-mapping", &note, &parts)
                        .await?;
                    out.counters.quarantined_records += 1;
                    out.counters.quarantined_bytes += bytes;
                    fire_repair_abort_hook(&what)?;
                    if !flip_mapping_damaged(ctx, *ino, *block_idx, mapping).await? {
                        refuse(
                            &mut out,
                            f,
                            "mapping superseded between verify and flip (foreground \
                             rewrite / mover publish) — nothing to quarantine"
                                .to_string(),
                        );
                        continue;
                    }
                    apply_ok(
                        &mut out,
                        f,
                        "quarantine-mapping",
                        format!(
                            "ino {ino} block {block_idx}: mapping replaced with the \
                             explicit damaged marker (reads EIO; loss made visible, \
                             never a fabricated hole)"
                        ),
                    );
                }
            }
            // ------------------------------------------------------- C3
            FindingId::C3Refcount { vol, offset } => {
                let Some(alloc) = alloc_of(vol) else {
                    refuse(&mut out, f, format!("volume '{vol}' no longer registered"));
                    continue;
                };
                let fresh = fresh.as_ref().expect("census walked");
                // The referencing inos, ascending — the §5.6a lease order.
                let mut referencers: Vec<u64> = fresh
                    .mappings
                    .iter()
                    .filter(|m| &m.vol == vol && m.offset == *offset)
                    .map(|m| m.ino)
                    .collect();
                referencers.sort_unstable();
                referencers.dedup();
                if referencers.is_empty() {
                    refuse(
                        &mut out,
                        f,
                        "no referencers remain (the unreferenced shape is C2's business)"
                            .to_string(),
                    );
                    continue;
                }
                // Take every referencing ino's exclusive lease (ascending)
                // and recount UNDER them.
                let mut guards = Vec::with_capacity(referencers.len());
                if online {
                    for &g_ino in &referencers {
                        let (vol_idx, local) = ctx.meta.route_ino(g_ino);
                        if let Some(kv) = ctx.meta.volumes.get(vol_idx) {
                            guards.push(kv.dlm().lock_inode_exclusive(local).await);
                        }
                    }
                }
                let mut counted = 0u32;
                for &r_ino in &referencers {
                    let (vol_idx, local) = ctx.meta.route_ino(r_ino);
                    let Some(kv) = ctx.meta.volumes.get(vol_idx) else {
                        continue;
                    };
                    let Ok(Some(bytes)) = kv.getxattr(local, "layout").await else {
                        continue;
                    };
                    let layout: Option<crate::routing::LayoutMetadata> = if bytes.starts_with(b"{")
                    {
                        serde_json::from_slice(&bytes).ok()
                    } else {
                        bincode::deserialize(&bytes).ok()
                    };
                    let Some(layout) = layout else { continue };
                    let mut probe = probe_census();
                    let tree_entries = kvmap_entries_for(ctx, kv, local, &layout).await;
                    census_layout(ctx, r_ino, &layout, block_size, tree_entries, &mut probe).await;
                    counted += probe
                        .refs
                        .get(vol)
                        .and_then(|m| m.get(offset))
                        .copied()
                        .unwrap_or(0);
                }
                let actual = alloc.refcount(*offset);
                match actual {
                    Some(actual) if counted > 0 && actual != counted => {
                        fire_repair_abort_hook(&what)?;
                        alloc.fsck_set_refcount(*offset, counted);
                        drop(guards);
                        apply_ok(
                            &mut out,
                            f,
                            "recount-and-set-refcount",
                            format!(
                                "{vol}:{offset} refcount {actual} → {counted} (recounted \
                                 under {} referencing lease(s))",
                                referencers.len()
                            ),
                        );
                    }
                    Some(actual) if counted > 0 => {
                        drop(guards);
                        refuse(
                            &mut out,
                            f,
                            format!("refcount {actual} already matches the recount (healed)"),
                        );
                    }
                    _ => {
                        drop(guards);
                        refuse(
                            &mut out,
                            f,
                            "offset untracked or unreferenced at recount (C2's business)"
                                .to_string(),
                        );
                    }
                }
            }
            // ------------------------------------------------------- C4
            FindingId::C4Orphan { dir, key, ino } => {
                // Verify: custody still live AND the ino still has no
                // meta (v3 inos are monotonic — never reused — so a
                // missing ino can never come back; the lease is for the
                // read's coherence online).
                let still_present =
                    match crate::cache::nvme::scan_live_staged_custody(dir, CUSTODY_SCAN_MAX).await
                    {
                        Ok(keys) => keys.iter().any(|k| k == key),
                        Err(e) => {
                            refuse(&mut out, f, format!("custody scan failed at verify: {e}"));
                            continue;
                        }
                    };
                if !still_present {
                    refuse(
                        &mut out,
                        f,
                        "custody record no longer present (flushed / already discarded)"
                            .to_string(),
                    );
                    continue;
                }
                let (vol_idx, local) = ctx.meta.route_ino(*ino);
                let still_missing = {
                    let _lease = if online {
                        Some(
                            ctx.meta.volumes[vol_idx]
                                .dlm()
                                .lock_inode_exclusive(local)
                                .await,
                        )
                    } else {
                        None
                    };
                    match ctx.meta.volumes[vol_idx].getattr(local).await {
                        Ok(inode) => inode.nlink == 0,
                        Err(e) if is_not_found(&e) => true,
                        Err(_) => false,
                    }
                };
                if !still_missing {
                    refuse(
                        &mut out,
                        f,
                        "ino has live meta now (not an orphan)".to_string(),
                    );
                    continue;
                }
                // Quarantine the record image(s) — header + key + payload.
                let images =
                    crate::cache::nvme::extract_and_kill_staged_custody(dir, key, false).await?;
                if images.is_empty() {
                    refuse(
                        &mut out,
                        f,
                        "custody record vanished between verify and quarantine".to_string(),
                    );
                    continue;
                }
                let parts: Vec<(&str, &[u8])> =
                    images.iter().map(|img| ("record", img.as_ref())).collect();
                let bytes = quarantine
                    .put(
                        &f.class,
                        &f.object,
                        "quarantine-then-discard-custody",
                        "verbatim staged record image(s): header + custody key + payload",
                        &parts,
                    )
                    .await?;
                out.counters.quarantined_records += images.len() as u64;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                // Discard: live store first (zeroes its indexed record),
                // then the raw on-disk residue (seeded / unindexed images).
                // Blocking-pool hop (shard-lock invariant rule 2): live-lane
                // fsck runs on executor threads, and the shard WRITE lock
                // legitimately waits for §5.5 read guards with await-side
                // lifetimes (the VL8 generic/464 wedge family).
                let _ = ctx
                    .router
                    .cache
                    .nvme
                    .remove_active_block_async(key.to_string())
                    .await;
                let _ = crate::cache::nvme::extract_and_kill_staged_custody(dir, key, true).await?;
                apply_ok(
                    &mut out,
                    f,
                    "quarantine-then-discard-custody",
                    format!(
                        "orphan custody '{key}' discarded from {} ({} record image(s) \
                         quarantined first)",
                        dir.display(),
                        images.len()
                    ),
                );
            }
            // ------------------------------------------------------- C5
            FindingId::C5Staging { dir } => {
                let Some(expected) = ctx.expected_generation.as_deref() else {
                    refuse(
                        &mut out,
                        f,
                        "no expected volume-set generation in this context".to_string(),
                    );
                    continue;
                };
                let stale = match crate::cache::nvme::read_staging_generation_marker(dir).await {
                    Ok(Some(found)) => !crate::writer_scope::marker_is_rebindable(&found, expected),
                    Ok(None) => {
                        crate::cache::nvme::dir_has_segment_data(&dir.join("staging_segment"))
                    }
                    Err(_) => true,
                };
                if !stale {
                    refuse(
                        &mut out,
                        f,
                        "staging generation matches the mounted set now (restamped)".to_string(),
                    );
                    continue;
                }
                // Quarantine: copy the marker + every staging segment file
                // aside (move = copy + fsync + remove; the copy is durable
                // BEFORE anything is removed).
                let mut parts_owned: Vec<(String, Vec<u8>)> = Vec::new();
                let marker_path = dir.join(crate::cache::nvme::STAGING_GENERATION_MARKER);
                if let Ok(bytes) = crate::uring_fs::read_all(&marker_path).await {
                    parts_owned.push(("generation_marker".to_string(), bytes.to_vec()));
                }
                let seg_dir = dir.join("staging_segment");
                let mut seg_files: Vec<PathBuf> = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&seg_dir) {
                    for entry in entries.flatten() {
                        if entry.metadata().map(|m| m.is_file()).unwrap_or(false) {
                            seg_files.push(entry.path());
                        }
                    }
                }
                for p in &seg_files {
                    if let Ok(bytes) = crate::uring_fs::read_all(p).await {
                        let name = p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "segment".to_string());
                        parts_owned.push((format!("segment_{name}"), bytes.to_vec()));
                    }
                }
                let parts: Vec<(&str, &[u8])> = parts_owned
                    .iter()
                    .map(|(n, b)| (n.as_str(), b.as_slice()))
                    .collect();
                let bytes = quarantine
                    .put(
                        &f.class,
                        &f.object,
                        "quarantine-staging-dir",
                        "stale-generation marker + staged segment files, moved aside \
                         (copy retained — the discard-on-mismatch law made non-destructive)",
                        &parts,
                    )
                    .await?;
                out.counters.quarantined_records += parts_owned.len() as u64;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                // The move-aside completes: remove the originals (the
                // copies above are durable).
                let _ = std::fs::remove_file(&marker_path);
                for p in &seg_files {
                    let _ = std::fs::remove_file(p);
                }
                apply_ok(
                    &mut out,
                    f,
                    "quarantine-staging-dir",
                    format!(
                        "{}: stale marker + {} segment file(s) moved aside",
                        dir.display(),
                        seg_files.len()
                    ),
                );
            }
            // ------------------------------------------------------- C6
            FindingId::C6Drift { vol } => {
                let Some(alloc) = alloc_of(vol) else {
                    refuse(&mut out, f, format!("volume '{vol}' no longer registered"));
                    continue;
                };
                let used = alloc
                    .highest_block_index()
                    .saturating_sub(alloc.free_blocks_count());
                let tracked = alloc.tracked_offsets().len() as u64;
                if used == tracked {
                    refuse(
                        &mut out,
                        f,
                        "accounting converged on its own (healed)".to_string(),
                    );
                    continue;
                }
                fire_repair_abort_hook(&what)?;
                let (frees_completed, evictions) = alloc.fsck_reconcile_accounting();
                apply_ok(
                    &mut out,
                    f,
                    "recompute-accounting",
                    format!(
                        "{vol}: derived accounting recomputed from the tracked census \
                         ({frees_completed} wedged free(s) completed, {evictions} \
                         free-list eviction(s))"
                    ),
                );
            }
            // ------------------------------------------------------- C7
            FindingId::C7Scrub {
                ino,
                block_idx,
                mapping,
            } => {
                if !current_mapping_present(ctx, *ino, *block_idx, mapping).await {
                    refuse(
                        &mut out,
                        f,
                        "mapping no longer present (rewritten / truncated / already \
                         quarantined)"
                            .to_string(),
                    );
                    continue;
                }
                let probe = {
                    let clean = clean_key(mapping);
                    let (pvol, poff) = ctx
                        .router
                        .backend_router
                        .parse_block_key(&clean)
                        .ok()
                        .and_then(|(be, off)| canonical_backend(ctx, &be).map(|(v, _)| (v, off)))
                        .unwrap_or_else(|| ("?".to_string(), 0));
                    MappingRef {
                        ino: *ino,
                        block_idx: *block_idx,
                        mapping: mapping.clone(),
                        vol: pvol,
                        offset: poff,
                        damaged: false,
                    }
                };
                let still_failing = if online {
                    reverify_scrub_failure(ctx, &crypto, &probe, block_size).await
                } else {
                    matches!(
                        verify_stored_block(ctx, &crypto, &probe, block_size).await,
                        ScrubOutcome::Failed(_)
                    )
                };
                if !still_failing {
                    refuse(
                        &mut out,
                        f,
                        "block verifies now (moved / rewritten since the scan)".to_string(),
                    );
                    continue;
                }
                // Quarantine what is readable, then isolate the mapping.
                let image = read_stored_image_best_effort(ctx, mapping).await;
                let (note, parts): (String, Vec<(&str, &[u8])>) = match &image {
                    Some(img) => (
                        "corrupt stored image (verbatim device window) — the physical \
                         block also stays in place for forensics"
                            .to_string(),
                        vec![("block", img.as_ref())],
                    ),
                    None => (
                        "stored image unreadable (device read error) — identity \
                         recorded; the physical block stays in place"
                            .to_string(),
                        Vec::new(),
                    ),
                };
                let bytes = quarantine
                    .put(&f.class, &f.object, "quarantine-mapping", &note, &parts)
                    .await?;
                out.counters.quarantined_blocks += 1;
                out.counters.quarantined_bytes += bytes;
                fire_repair_abort_hook(&what)?;
                if !flip_mapping_damaged(ctx, *ino, *block_idx, mapping).await? {
                    refuse(
                        &mut out,
                        f,
                        "mapping superseded between verify and flip (foreground \
                         rewrite / mover publish) — the block moved on"
                            .to_string(),
                    );
                    continue;
                }
                apply_ok(
                    &mut out,
                    f,
                    "quarantine-mapping",
                    format!(
                        "ino {ino} block {block_idx}: mapping quarantined (reads EIO); \
                         physical block preserved for forensics"
                    ),
                );
            }
        }
    }

    out.quarantine_dir = quarantine.run_dir.as_ref().map(|d| d.display().to_string());
    publish_repair_metrics(&out.counters);
    Ok(out)
}

fn publish_repair_metrics(c: &RepairCounters) {
    use crate::fuse_client::METRICS;
    let m = &*METRICS;
    m.fsck_repairs_planned
        .fetch_add(c.planned, Ordering::Relaxed);
    m.fsck_repairs_applied
        .fetch_add(c.applied, Ordering::Relaxed);
    m.fsck_repairs_refused
        .fetch_add(c.refused, Ordering::Relaxed);
    m.fsck_repair_refused_multi_owner
        .fetch_add(c.refused_multi_owner, Ordering::Relaxed);
    m.fsck_quarantined_records
        .fetch_add(c.quarantined_records, Ordering::Relaxed);
    m.fsck_quarantined_blocks
        .fetch_add(c.quarantined_blocks, Ordering::Relaxed);
    m.fsck_quarantined_bytes
        .fetch_add(c.quarantined_bytes, Ordering::Relaxed);
    // per_class applied counts are published inline at apply time (the
    // per-action `apply_ok` path) — not re-added here.
}

// ---------------------------------------------------------------------------
// Offline harness (read-only probes; §5.8 duality)
// ---------------------------------------------------------------------------

/// The offline CLI harness: refuse under a live writer, open read-only
/// probes, build a probe-shaped router, rebuild the allocator census
/// (full runs only — shards skip the allocator-dependent classes by
/// design), run the engine, release the probes.
pub async fn run_offline(meta_lvs: &[String], opts: &FsckOptions) -> Result<FsckReport> {
    use std::path::Path;
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "offline fsck refused: {e} — run `squeezefs fsck <mountpoint>` against \
                     the live mount instead (offline mode requires nothing in flight, §5.6)"
                ))
            })?;
    }
    let routed = crate::meta_backend::open_probe_routed_meta_set(meta_lvs).await?;
    let result = run_offline_body(&routed, meta_lvs, opts, None).await;
    for vol in &routed.volumes {
        if let Err(e) = vol.shutdown().await {
            log::warn!("releasing probe after offline fsck: {e}");
        }
    }
    result
}

/// The offline `--repair` harness (§5.6a / §5.8): repair is a WRITER —
/// it runs under the **D0-guarded open** (the same posture every offline
/// mutating verb takes — `set-cache-paths` / `volume remove-data`),
/// never the read-only probe. Refused on `--shards` probe shards (a
/// shard sees a partial census; repair acts only on whole-scan
/// findings). Detection runs first inside the guard; the repair
/// (dry-run or apply per `ropts`) consumes its verified findings; the
/// combined report is returned with [`FsckReport::repair`] populated.
pub async fn run_offline_repair(
    meta_lvs: &[String],
    opts: &FsckOptions,
    ropts: &RepairOptions,
) -> Result<FsckReport> {
    use std::path::Path;
    if opts.shard.is_some() {
        return Err(SqueezefsError::InvalidOperation(
            "--repair is refused on --shards probe shards: repair requires the whole-scan \
             findings under the guarded open (§5.6a); run the repair unsharded"
                .to_string(),
        ));
    }
    for path in meta_lvs {
        crate::meta_backend::kv::builder::format_preflight(Path::new(path), true)
            .await
            .map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "offline fsck --repair refused: {e} — repair requires exclusive \
                     guarded access (§5.6a)"
                ))
            })?;
    }
    // The D0-guarded open (writer claims) — NOT the read-only probe.
    let routed = crate::meta_backend::open_routed_meta_set(meta_lvs).await?;
    let result = run_offline_body(&routed, meta_lvs, opts, Some(ropts)).await;
    for vol in &routed.volumes {
        if let Err(e) = vol.shutdown().await {
            log::warn!("releasing guard after offline fsck --repair: {e}");
        }
    }
    result
}

/// The offline body over a set the CALLER opened (a probe or a guarded
/// writer) and releases — the harness form of [`run_offline`] /
/// [`run_offline_repair`], which add the live-client preflight, the open
/// and the release around it. A crashed holder's `writer_claim` stays
/// heartbeat-fresh for the TTL, so a contract judging a set right after a
/// kill reaches the census through this door with the probe it holds.
pub async fn run_offline_over(
    routed: &Arc<RoutedMetaBackend>,
    meta_lvs: &[String],
    opts: &FsckOptions,
    repair_opts: Option<&RepairOptions>,
) -> Result<FsckReport> {
    run_offline_body(routed, meta_lvs, opts, repair_opts).await
}

async fn run_offline_body(
    routed: &Arc<RoutedMetaBackend>,
    meta_lvs: &[String],
    opts: &FsckOptions,
    repair_opts: Option<&RepairOptions>,
) -> Result<FsckReport> {
    let cfg = crate::config_ops::read_volume_format_config(meta_lvs).await?;
    let records = cfg.resolved_data_volumes();
    let dlm = crate::dlm::DlmClient::new()?;
    let live: Vec<&crate::DataVolumeRecord> = records
        .iter()
        .filter(|r| r.state != crate::VOL_STATE_RETIRED)
        .collect();
    let first = live.first().ok_or_else(|| {
        SqueezefsError::InvalidOperation("no live data volumes to fsck".to_string())
    })?;
    let first_alloc = Arc::new(crate::block_allocator::BlockAllocator::new(&first.id).await?);
    if let Ok(cap) = crate::nvme_dev::device_capacity_bytes(&first.backing_dev) {
        first_alloc.set_capacity_bytes(cap);
    }
    let first_dev = Arc::new(crate::nvme_dev::NvmeBlockDev::new(&first.backing_dev));
    let cache = crate::cache::TieredCache::new(
        Vec::new(), // never adopt/mutate the mount's staging dirs
        Some("64MB"),
        Some("64MB"),
        None,
        None,
        first_alloc.clone(),
        first_dev.clone(),
        None,
    )
    .await?;
    let router = crate::routing::DataRouter::new(dlm.clone(), cache, first_alloc, first_dev);
    // §3d.2 (rc-manifest): block size rides the router seams, never a
    // process-env write (see `config_ops::offline_drain_body` — the same
    // retired runtime channel). The passthrough `set_crypto` pins the
    // DUR-8e plaintext bound to this set's block size.
    router.set_block_size(cfg.block_size);
    router.set_crypto(crate::crypto_compress::CryptoCompressState::new(
        "none".to_string(),
        "none".to_string(),
        None,
    ));
    for rec in &live {
        router.backend_router.register_backend(rec).await?;
    }
    router.backend_router.set_volume_records(records.clone());
    router.set_meta_backend(routed.clone());

    // Full runs rebuild the allocator census (the C2/C3 ground truth an
    // unmounted set can offer); shards skip it — their allocator-side
    // classes finalize at merge from the partial censuses.
    if opts.shard.is_none() {
        for kv in &routed.volumes {
            for entry in router.backend_router.backends.iter() {
                entry
                    .value()
                    .block_allocator
                    .recover_active_blocks_v3(kv, &router.backend_router)
                    .await?;
            }
        }
    }

    // The config records the staging ROOTS; a mount isolates its actual
    // staging under `<root>/squeezefs/<sanitized-mountpoint>/` (the
    // per-mount isolation in `main`'s mount path — the generation marker
    // and `staging_segment/` live THERE, not at the root). Offline
    // C4/C5 must scan both shapes: the raw root (legacy/test fixtures,
    // and the quarantine home stays rooted there) plus every isolated
    // per-mount dir found under it (`cache_segment` is the shared read
    // cache — no custody, no marker — and scans inert either way).
    let staging_roots = crate::config_ops::get_cache_paths(meta_lvs)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut staging_dirs = Vec::new();
    for root in staging_roots {
        staging_dirs.push(root.clone());
        if let Ok(entries) = std::fs::read_dir(root.join("squeezefs")) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && entry.file_name() != "cache_segment"
                {
                    staging_dirs.push(entry.path());
                }
            }
        }
    }
    let ctx = FsckCtx {
        meta: routed.clone(),
        router,
        staging_dirs,
        expected_generation: Some(volume_generation(routed)),
    };
    let mut report = run(&ctx, opts).await?;
    if let Some(ropts) = repair_opts {
        report.repair = Some(repair(&ctx, &report, ropts).await?);
    }
    Ok(report)
}
