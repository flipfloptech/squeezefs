//! **The appender page, its A/B pair and the appender directory**
//! (docs/design-symmetric-metadata.md §5.3 B(ii); incompat bit 17, PR 2).
//!
//! Under the slot-tree forest every metadata writer is an **appender**: it
//! owns a journal ring (a segment table of heap extents — appender 0's is
//! the format-time fixed ring), a 4 KiB **page** that is at once its
//! directory entry and its ledger record (identity, term, state, ring
//! segments, journal tail, checkpoint seq, extent grant, the slots it
//! leases with their roots and cursors), and an extent grant. The page is
//! written once per checkpoint and read once per recovery; it never names
//! tree 0's root or the membership stamp — those stay the fixed ledger's,
//! so two managers are unrepresentable (KD-SYM-3).
//!
//! ## Where pages live
//!
//! Appender **0**'s page pair occupies the first two pages of the fixed
//! journal extent and its two ring-side ledger pages the next two
//! ([`APPENDER0_RESERVED_PAGES`]); its ring proper starts after them
//! ([`appender0_ring_extent`]). Appenders ≥ 1 live in the **appender
//! directory**, a chain of heap extents named by
//! `superblock.appender_dir`: each extent holds page PAIRS from its first
//! page up and a **header page** LAST ([`DirHeader`] — magic, chain
//! index, `next`), so a directory of `node_size` bytes holds
//! `(node_size / 4096 − 1) / 2` pairs ([`dir_pairs_per_extent`]). An
//! appender's two ring-side pages are the first two pages of its ring's
//! first segment ([`RING_SIDE_LEDGER_PAGES`]).
//!
//! ## Newest-valid-wins
//!
//! Four page slots per appender — the directory pair `[A, B]` and the
//! ring-side pair `[R0, R1]` — written round-robin by generation
//! ([`page_slot_for`]); the reader takes the valid image with the highest
//! generation ([`newest_valid`]). A torn newest page therefore falls back
//! to a predecessor of the SAME appender with two of slack (§5.3.4's
//! "torn appender page" row).
//!
//! ## Decode is total
//!
//! [`AppenderPage::decode`] answers a typed error on a short, blank,
//! bad-magic, bad-checksum or structurally impossible image — never a
//! panic and never an allocation the image's own counts drive (every
//! count is checked against its bound before an element is read). This
//! codec is fuzzed (`fuzz/fuzz_targets/appender_page.rs`) and mirrored on
//! stable in `tests/decoder_property_tests.rs`.

use super::superblock::{ExtentRef, SECTOR_SIZE};
use super::tree::RootPtr;
use super::KvError;
use std::path::Path;

/// One appender page: the metadata sector (4 KiB, the journal page size).
pub const APPENDER_PAGE_LEN: usize = SECTOR_SIZE;
/// Page magic (`"KVAP"`).
pub const APPENDER_PAGE_MAGIC: u32 = 0x4B56_4150;
/// Directory header-page magic (`"KVAD"`).
pub const APPENDER_DIR_MAGIC: u32 = 0x4B56_4144;

/// Ring segments a page names: 8 — one 256 KiB segment is 64 pages, so
/// eight is the 2 MiB largest step a ring grows per `journal_full_stalls`
/// event before the page's remaining bytes would bind (§5.3.2).
pub const RING_SEGMENTS_MAX: usize = 8;
/// Extent-grant runs a page names: 4 — the current run, its successor and
/// two returns in flight (the `alloc_lane` ahead-refill's own depth, §5.3.2).
pub const GRANT_RUNS_MAX: usize = 4;

/// Page slots per appender: the directory pair + the two ring-side ledger
/// pages (§5.3.2 — a torn newest page has two predecessors, with slack).
pub const APPENDER_PAGE_SLOTS: usize = 4;
/// Pages of the fixed journal extent appender 0's page slots occupy: its
/// `[A, B]` pair + its `[R0, R1]` ring-side pair.
pub const APPENDER0_RESERVED_PAGES: u64 = APPENDER_PAGE_SLOTS as u64;
/// Pages at the head of an appender ≥ 1's FIRST ring segment that hold its
/// ring-side ledger pages `[R0, R1]`.
pub const RING_SIDE_LEDGER_PAGES: u64 = 2;

// ---- The page's fixed part, LE, offsets in order -------------------------
const OFF_MAGIC: usize = 0; // u32
const OFF_APPENDER_ID: usize = 4; // u32
const OFF_GENERATION: usize = 8; // u64
const OFF_CHECKSUM: usize = 16; // u64, xxh3_64 of the image with this field zeroed
const OFF_NODE_TOKEN: usize = 24; // u64
const OFF_WRITER_ID: usize = 32; // u128
const OFF_MOUNT_SLOT: usize = 48; // u32 — the KD-MW-2 slot at its shipped width
const OFF_STATE: usize = 52; // u8
const OFF_IS_MANAGER: usize = 53; // u8
const OFF_HOME_VOLUME: usize = 54; // u8
const OFF_LAYOUT_VERSION: usize = 55; // u8 — APPENDER_PAGE_LAYOUT_VERSION (PR 4)
const OFF_TERM: usize = 56; // u64
const OFF_RECOVERED_BY_TERM: usize = 64; // u64
const OFF_N_SEGMENTS: usize = 72; // u16
const OFF_RESERVED_74: usize = 74; // [u8; 6], zero (aligns the segment table)
const OFF_SEGMENTS: usize = 80; // [ExtentRef; 8] × 16 B
const SEGMENT_ENC_LEN: usize = 16;
const OFF_HEAD_HINT: usize = OFF_SEGMENTS + RING_SEGMENTS_MAX * SEGMENT_ENC_LEN; // 208, u64
const OFF_LEDGER_TAIL_SEQ: usize = OFF_HEAD_HINT + 8; // 216, u64
const OFF_CKPT_SEQ: usize = OFF_LEDGER_TAIL_SEQ + 8; // 224, u64
const OFF_N_RUNS: usize = OFF_CKPT_SEQ + 8; // 232, u16
const OFF_RESERVED_234: usize = 234; // [u8; 6], zero (aligns the run table)
const OFF_RUNS: usize = 240; // [(start u64, len u32, reserved u32); 4] × 16 B
const RUN_ENC_LEN: usize = 16;
const OFF_N_SLOTS: usize = OFF_RUNS + GRANT_RUNS_MAX * RUN_ENC_LEN; // 304, u16
const OFF_SEQ_OFFSET: usize = OFF_N_SLOTS + 2; // 306, u64 — the ring's record-seq offset (PR 4)
const OFF_SLOTS: usize = OFF_SEQ_OFFSET + 8; // 314

/// The page LAYOUT version (byte 55, a reserved-zero byte through PR 3):
/// `1` = PR 4's layout — `seq_offset` at 306 and the slot entries at 314.
/// The PR 2/3 layout (slots at 306, no offset word) wrote 0 here and
/// decodes REFUSED, never misread: a page under the same magic with the
/// slot table 8 bytes earlier would otherwise verify its checksum and
/// hand back a `seq_offset` made of slot bytes (review round 3, Issue
/// 23). Bit 17 is stamped by no field volume, so no such page exists
/// outside a test tempdir; the byte is what makes the next move loud.
pub const APPENDER_PAGE_LAYOUT_VERSION: u8 = 1;

/// Bytes of the page before its slot entries — the field list of §5.3.2
/// at natural widths (the KD-MW-2 mount slot at its shipped u32).
pub const APPENDER_PAGE_FIXED_LEN: usize = OFF_SLOTS;
/// One leased-slot entry: `slot: u16 ‖ state: u8 ‖ g: u32 ‖
/// slot_tree_extents: u32 ‖ root.addr: u64 ‖ root.seq: u64 ‖ cursor: u64`.
pub const SLOT_ENTRY_LEN: usize = 2 + 1 + 4 + 4 + 8 + 8 + 8;
/// Leased-slot entries one page holds — DERIVED from the page's own
/// fixed part and entry width, never a free constant; beyond it the
/// holder LRU-releases idle slots (§5.1.3, PR 4).
pub const SLOT_PAGE_BUDGET: usize = (APPENDER_PAGE_LEN - APPENDER_PAGE_FIXED_LEN) / SLOT_ENTRY_LEN;

/// An appender's lifecycle state (§5.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AppenderState {
    /// Never joined, or released — the page is allocatable.
    Free = 0,
    /// Joined; its ring may hold in-window records.
    Live = 1,
    /// A recoverer is replaying its ring (§5.9).
    Recovering = 2,
    /// Recovered; its ring is never replayed again (§5.8.3) — re-adopted
    /// with a bumped term and a fresh ring head.
    Recovered = 3,
}

impl AppenderState {
    fn from_u8(v: u8) -> Result<Self, KvError> {
        Ok(match v {
            0 => Self::Free,
            1 => Self::Live,
            2 => Self::Recovering,
            3 => Self::Recovered,
            other => {
                return Err(KvError::Corrupt(format!(
                    "appender page carries unknown state {other}"
                )))
            }
        })
    }

    /// The operator-facing word.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Live => "live",
            Self::Recovering => "recovering",
            Self::Recovered => "recovered",
        }
    }
}

/// A leased slot's entry state (§5.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SlotEntryState {
    Live = 0,
    /// Mid-handover: `root`/`cursor`/`g` are the departing holder's
    /// attestation; tree 0 wins over it (§5.3.4).
    Releasing = 1,
}

impl SlotEntryState {
    fn from_u8(v: u8) -> Result<Self, KvError> {
        Ok(match v {
            0 => Self::Live,
            1 => Self::Releasing,
            other => {
                return Err(KvError::Corrupt(format!(
                    "appender page slot entry carries unknown state {other}"
                )))
            }
        })
    }
}

/// The appender's identity: the KD-MW-2 client scope plus the writer's
/// per-mount id (`WriterClaim.id` as a u128).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AppenderIdentity {
    pub node_token: u64,
    pub mount_slot: u32,
    pub writer_id: u128,
}

impl AppenderIdentity {
    /// Is a page stamped with this identity OUR residue, judged by a
    /// writer that holds the D0 flock (§5.3.2 identity binding, the "D0
    /// `Reclaimable` arm")?
    ///
    /// The flock is the kernel's same-HOST death proof, so under D0 a
    /// `Live` page of this node — whatever mount point (slot) its holder
    /// derived — can only be a dead predecessor's: the kill-9 successor
    /// remounting at another mount point is the shipped same-host
    /// crash-remount shape, and the flat path reclaims it instantly. The
    /// writer id changes per mount and never binds.
    ///
    /// **The proof reaches page 0 alone since PR 12b** (the N-daemon
    /// posture): the flock is held by the MANAGER, whose page is page 0 —
    /// every other page is a JOINED appender's, which holds no flock, so
    /// a same-node `Live` page at another mount slot there is a LIVE
    /// daemon of this host as readily as a dead one. Those pages are
    /// judged by [`Self::is_mount`] (the exact `(node, mount slot)`
    /// binding) and a dead one is the death ledger's (PR 10) — see
    /// [`page_is_own`].
    pub fn owned_by_node(&self, node_token: u64) -> bool {
        self.node_token == node_token
    }

    /// The exact KD-MW-2 mount identity — `(node_token, mount_slot)`; the
    /// per-open writer id never binds.
    pub fn is_mount(&self, node_token: u64, mount_slot: u32) -> bool {
        self.node_token == node_token && self.mount_slot == mount_slot
    }
}

/// **The ONE "is this page OURS" predicate** (§5.3.2 identity binding as
/// PR 12b narrows it): page 0 of a WRITER open is this node's whatever
/// mount slot it carries (the D0 flock is the same-host proof for the
/// manager's page — `AppenderIdentity::owned_by_node`); every other page
/// — and every page on a JOINED open — is ours iff it carries this exact
/// `(node, mount slot)`. Before the narrowing every same-node page was
/// own residue, which under N daemons on one host would have adopted a
/// LIVE joiner's ring at the manager's remount, floored ring 0 on its
/// leased slots for the mount's life (the PR 10 Issue-32 wedge, reachable
/// again) and named its allocation lease dead at the arm.
pub fn page_is_own(
    appender_id: u32,
    page_identity: &AppenderIdentity,
    own_node: u64,
    own_slot: u32,
    writer_open: bool,
) -> bool {
    if appender_id == 0 && writer_open {
        page_identity.owned_by_node(own_node)
    } else {
        page_identity.is_mount(own_node, own_slot)
    }
}

/// One leased slot on a page (§5.3.2). `slot` is the ROUTING slot
/// (`u16`); the forest slot is derived against the volume's native slot
/// ([`forest_slot_of_page_slot`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotEntry {
    pub slot: u16,
    pub state: SlotEntryState,
    /// The slot's lease generation (§5.8.2).
    pub g: u32,
    /// The slot tree's durable size for the affinity cap (§5.1.2).
    pub slot_tree_extents: u32,
    pub root: RootPtr,
    pub cursor: u64,
}

/// One extent-grant run: `len` extents from extent `start` (§5.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantRun {
    pub start: u64,
    pub len: u32,
}

/// The appender page — the 4 KiB LE image of §5.3.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppenderPage {
    pub appender_id: u32,
    /// Bumped at every write; the newest-valid-wins key.
    pub generation: u64,
    pub identity: AppenderIdentity,
    /// The S2 writer era per appender, bumped at every (re)join.
    pub term: u64,
    pub state: AppenderState,
    /// The recoverer's term (`Recovering` / `Recovered`).
    pub recovered_by_term: u64,
    /// KD-SYM-3: the manager is an ordinary appender with this bit.
    pub is_manager: bool,
    pub home_volume: u8,
    /// Ring segments in order (≤ [`RING_SEGMENTS_MAX`]).
    pub segments: Vec<ExtentRef>,
    /// The ring head at the last page write (a hint; the ring scan owns
    /// the truth).
    pub head_hint: u64,
    /// This ring's replay tail — the appender's ledger record.
    pub ledger_tail_seq: u64,
    pub ckpt_seq: u64,
    /// The ring's record-seq OFFSET in force at the page write
    /// (`JournalRing::seq_offset` — the seq-space law, design §5.1.4 /
    /// §5.8.2, PR 4): a record's seq is its position plus this; a grant
    /// of a slot whose departing ring stamped past this ring's frontier
    /// raises it. Recovered as the max of this and the window's stamps.
    /// 0 for a ring nothing was ever handed to.
    pub seq_offset: u64,
    /// Extent-grant runs (≤ [`GRANT_RUNS_MAX`]; empty until PR 3 grants).
    pub grant: Vec<GrantRun>,
    /// Leased slots (≤ [`SLOT_PAGE_BUDGET`]), slot-ascending.
    pub slots: Vec<SlotEntry>,
}

impl AppenderPage {
    /// A `Free` page for `appender_id` — what format and a region release
    /// write. Names no identity, no ring, no slots.
    pub fn free(appender_id: u32, generation: u64) -> Self {
        Self {
            appender_id,
            generation,
            identity: AppenderIdentity::default(),
            term: 0,
            state: AppenderState::Free,
            recovered_by_term: 0,
            is_manager: false,
            home_volume: 0,
            segments: Vec::new(),
            head_hint: 0,
            ledger_tail_seq: 0,
            ckpt_seq: 0,
            seq_offset: 0,
            grant: Vec::new(),
            slots: Vec::new(),
        }
    }

    /// The LE image (exactly [`APPENDER_PAGE_LEN`] bytes, checksummed).
    /// Refuses a page whose vectors exceed their bounds — a caller bug,
    /// never truncated.
    pub fn encode(&self) -> Result<Vec<u8>, KvError> {
        if self.segments.len() > RING_SEGMENTS_MAX {
            return Err(KvError::Corrupt(format!(
                "appender page cannot name {} ring segments (max {RING_SEGMENTS_MAX})",
                self.segments.len()
            )));
        }
        if self.grant.len() > GRANT_RUNS_MAX {
            return Err(KvError::Corrupt(format!(
                "appender page cannot name {} grant runs (max {GRANT_RUNS_MAX})",
                self.grant.len()
            )));
        }
        if self.slots.len() > SLOT_PAGE_BUDGET {
            return Err(KvError::Corrupt(format!(
                "appender page cannot name {} slots (SLOT_PAGE_BUDGET = {SLOT_PAGE_BUDGET})",
                self.slots.len()
            )));
        }
        let mut img = vec![0u8; APPENDER_PAGE_LEN];
        img[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&APPENDER_PAGE_MAGIC.to_le_bytes());
        img[OFF_APPENDER_ID..OFF_APPENDER_ID + 4].copy_from_slice(&self.appender_id.to_le_bytes());
        img[OFF_GENERATION..OFF_GENERATION + 8].copy_from_slice(&self.generation.to_le_bytes());
        img[OFF_NODE_TOKEN..OFF_NODE_TOKEN + 8]
            .copy_from_slice(&self.identity.node_token.to_le_bytes());
        img[OFF_WRITER_ID..OFF_WRITER_ID + 16]
            .copy_from_slice(&self.identity.writer_id.to_le_bytes());
        img[OFF_MOUNT_SLOT..OFF_MOUNT_SLOT + 4]
            .copy_from_slice(&self.identity.mount_slot.to_le_bytes());
        img[OFF_STATE] = self.state as u8;
        img[OFF_IS_MANAGER] = u8::from(self.is_manager);
        img[OFF_HOME_VOLUME] = self.home_volume;
        img[OFF_LAYOUT_VERSION] = APPENDER_PAGE_LAYOUT_VERSION;
        img[OFF_TERM..OFF_TERM + 8].copy_from_slice(&self.term.to_le_bytes());
        img[OFF_RECOVERED_BY_TERM..OFF_RECOVERED_BY_TERM + 8]
            .copy_from_slice(&self.recovered_by_term.to_le_bytes());
        img[OFF_N_SEGMENTS..OFF_N_SEGMENTS + 2]
            .copy_from_slice(&(self.segments.len() as u16).to_le_bytes());
        for (i, seg) in self.segments.iter().enumerate() {
            let off = OFF_SEGMENTS + i * SEGMENT_ENC_LEN;
            img[off..off + 8].copy_from_slice(&seg.start.to_le_bytes());
            img[off + 8..off + 16].copy_from_slice(&seg.len.to_le_bytes());
        }
        img[OFF_HEAD_HINT..OFF_HEAD_HINT + 8].copy_from_slice(&self.head_hint.to_le_bytes());
        img[OFF_LEDGER_TAIL_SEQ..OFF_LEDGER_TAIL_SEQ + 8]
            .copy_from_slice(&self.ledger_tail_seq.to_le_bytes());
        img[OFF_CKPT_SEQ..OFF_CKPT_SEQ + 8].copy_from_slice(&self.ckpt_seq.to_le_bytes());
        img[OFF_SEQ_OFFSET..OFF_SEQ_OFFSET + 8].copy_from_slice(&self.seq_offset.to_le_bytes());
        img[OFF_N_RUNS..OFF_N_RUNS + 2].copy_from_slice(&(self.grant.len() as u16).to_le_bytes());
        for (i, run) in self.grant.iter().enumerate() {
            let off = OFF_RUNS + i * RUN_ENC_LEN;
            img[off..off + 8].copy_from_slice(&run.start.to_le_bytes());
            img[off + 8..off + 12].copy_from_slice(&run.len.to_le_bytes());
        }
        img[OFF_N_SLOTS..OFF_N_SLOTS + 2].copy_from_slice(&(self.slots.len() as u16).to_le_bytes());
        for (i, e) in self.slots.iter().enumerate() {
            let off = OFF_SLOTS + i * SLOT_ENTRY_LEN;
            img[off..off + 2].copy_from_slice(&e.slot.to_le_bytes());
            img[off + 2] = e.state as u8;
            img[off + 3..off + 7].copy_from_slice(&e.g.to_le_bytes());
            img[off + 7..off + 11].copy_from_slice(&e.slot_tree_extents.to_le_bytes());
            img[off + 11..off + 19].copy_from_slice(&e.root.addr.to_le_bytes());
            img[off + 19..off + 27].copy_from_slice(&e.root.seq.to_le_bytes());
            img[off + 27..off + 35].copy_from_slice(&e.cursor.to_le_bytes());
        }
        let sum = page_checksum(&img);
        img[OFF_CHECKSUM..OFF_CHECKSUM + 8].copy_from_slice(&sum.to_le_bytes());
        Ok(img)
    }

    /// Decode + verify (total — see the module docs). Every reserved byte
    /// must be zero and every count within its bound, so whatever decodes
    /// re-encodes to the same bytes.
    pub fn decode(buf: &[u8]) -> Result<Self, KvError> {
        if buf.len() != APPENDER_PAGE_LEN {
            return Err(KvError::Corrupt(format!(
                "appender page must be {APPENDER_PAGE_LEN} bytes, got {}",
                buf.len()
            )));
        }
        if le32(buf, OFF_MAGIC) != APPENDER_PAGE_MAGIC {
            return Err(KvError::Corrupt("bad appender page magic".to_string()));
        }
        let stored = le64(buf, OFF_CHECKSUM);
        let computed = page_checksum(buf);
        if stored != computed {
            return Err(KvError::ChecksumMismatch { stored, computed });
        }
        if buf[OFF_LAYOUT_VERSION] != APPENDER_PAGE_LAYOUT_VERSION {
            return Err(KvError::Corrupt(format!(
                "appender page layout version {} — this binary writes \
                 {APPENDER_PAGE_LAYOUT_VERSION} (PR 4: seq_offset at {OFF_SEQ_OFFSET}, slot \
                 entries at {OFF_SLOTS}); a page of the PR 2/3 layout is refused, never \
                 misread (the format is forward-only: reformat the volume)",
                buf[OFF_LAYOUT_VERSION]
            )));
        }
        if buf[OFF_RESERVED_74..OFF_RESERVED_74 + 6]
            .iter()
            .any(|b| *b != 0)
            || buf[OFF_RESERVED_234..OFF_RESERVED_234 + 6]
                .iter()
                .any(|b| *b != 0)
        {
            return Err(KvError::Corrupt(
                "appender page reserved bytes are not zero".to_string(),
            ));
        }
        let n_segments = usize::from(le16(buf, OFF_N_SEGMENTS));
        if n_segments > RING_SEGMENTS_MAX {
            return Err(KvError::Corrupt(format!(
                "appender page names {n_segments} ring segments (max {RING_SEGMENTS_MAX})"
            )));
        }
        let n_runs = usize::from(le16(buf, OFF_N_RUNS));
        if n_runs > GRANT_RUNS_MAX {
            return Err(KvError::Corrupt(format!(
                "appender page names {n_runs} grant runs (max {GRANT_RUNS_MAX})"
            )));
        }
        let n_slots = usize::from(le16(buf, OFF_N_SLOTS));
        if n_slots > SLOT_PAGE_BUDGET {
            return Err(KvError::Corrupt(format!(
                "appender page names {n_slots} slots (SLOT_PAGE_BUDGET = {SLOT_PAGE_BUDGET})"
            )));
        }
        // Unused table entries and the tail padding must be zero (so the
        // image is canonical: decode ∘ encode = id).
        for i in n_segments..RING_SEGMENTS_MAX {
            let off = OFF_SEGMENTS + i * SEGMENT_ENC_LEN;
            if buf[off..off + SEGMENT_ENC_LEN].iter().any(|b| *b != 0) {
                return Err(KvError::Corrupt(
                    "appender page: an unused ring-segment entry is not zero".to_string(),
                ));
            }
        }
        for i in 0..GRANT_RUNS_MAX {
            let off = OFF_RUNS + i * RUN_ENC_LEN;
            let from = if i < n_runs { 12 } else { 0 };
            if buf[off + from..off + RUN_ENC_LEN].iter().any(|b| *b != 0) {
                return Err(KvError::Corrupt(
                    "appender page: grant-run padding is not zero".to_string(),
                ));
            }
        }
        if buf[OFF_SLOTS + n_slots * SLOT_ENTRY_LEN..]
            .iter()
            .any(|b| *b != 0)
        {
            return Err(KvError::Corrupt(
                "appender page: bytes past the last slot entry are not zero".to_string(),
            ));
        }
        let segments = (0..n_segments)
            .map(|i| {
                let off = OFF_SEGMENTS + i * SEGMENT_ENC_LEN;
                ExtentRef {
                    start: le64(buf, off),
                    len: le64(buf, off + 8),
                }
            })
            .collect();
        let grant = (0..n_runs)
            .map(|i| {
                let off = OFF_RUNS + i * RUN_ENC_LEN;
                GrantRun {
                    start: le64(buf, off),
                    len: le32(buf, off + 8),
                }
            })
            .collect();
        let mut slots = Vec::with_capacity(n_slots);
        for i in 0..n_slots {
            let off = OFF_SLOTS + i * SLOT_ENTRY_LEN;
            let e = SlotEntry {
                slot: le16(buf, off),
                state: SlotEntryState::from_u8(buf[off + 2])?,
                g: le32(buf, off + 3),
                slot_tree_extents: le32(buf, off + 7),
                root: RootPtr {
                    addr: le64(buf, off + 11),
                    seq: le64(buf, off + 19),
                },
                cursor: le64(buf, off + 27),
            };
            if let Some(prev) = slots.last() {
                let prev: &SlotEntry = prev;
                if prev.slot >= e.slot {
                    return Err(KvError::Corrupt(format!(
                        "appender page slot entries are not slot-ascending ({} then {})",
                        prev.slot, e.slot
                    )));
                }
            }
            slots.push(e);
        }
        Ok(Self {
            appender_id: le32(buf, OFF_APPENDER_ID),
            generation: le64(buf, OFF_GENERATION),
            identity: AppenderIdentity {
                node_token: le64(buf, OFF_NODE_TOKEN),
                mount_slot: le32(buf, OFF_MOUNT_SLOT),
                writer_id: u128::from_le_bytes(
                    buf[OFF_WRITER_ID..OFF_WRITER_ID + 16]
                        .try_into()
                        .map_err(|_| KvError::Corrupt("writer id slice".to_string()))?,
                ),
            },
            term: le64(buf, OFF_TERM),
            state: AppenderState::from_u8(buf[OFF_STATE])?,
            recovered_by_term: le64(buf, OFF_RECOVERED_BY_TERM),
            is_manager: match buf[OFF_IS_MANAGER] {
                0 => false,
                1 => true,
                other => {
                    return Err(KvError::Corrupt(format!(
                        "appender page is_manager byte is {other}, not 0/1"
                    )))
                }
            },
            home_volume: buf[OFF_HOME_VOLUME],
            segments,
            head_hint: le64(buf, OFF_HEAD_HINT),
            ledger_tail_seq: le64(buf, OFF_LEDGER_TAIL_SEQ),
            ckpt_seq: le64(buf, OFF_CKPT_SEQ),
            seq_offset: le64(buf, OFF_SEQ_OFFSET),
            grant,
            slots,
        })
    }

    /// The ring's byte length over its segments.
    pub fn ring_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.len).sum()
    }
}

/// xxh3_64 over the page with the checksum field zeroed.
/// xxh3 over a page image with its checksum field zeroed (the codec's
/// own; the contracts re-stamp a mutated image with it).
pub fn page_checksum(img: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&img[..OFF_CHECKSUM]);
    h.update(&[0u8; 8]);
    h.update(&img[OFF_CHECKSUM + 8..]);
    h.digest()
}

/// The page slot (0..4 over `[A, B, R0, R1]`) a write at `generation`
/// takes: round-robin, so the three predecessors stay intact.
pub fn page_slot_for(generation: u64) -> usize {
    (generation % APPENDER_PAGE_SLOTS as u64) as usize
}

/// How one page-slot image reads — THREE-way (PR 4 review round 4, Issue
/// 26): a page of another LAYOUT with a valid checksum is its own class,
/// never "torn". The four-slot reader falls back past `Blank` and
/// `Corrupt` to a predecessor; it REFUSES on `ForeignLayout` — a volume a
/// binary of another layout wrote is the forward-only law's "reformat
/// required", and the round-3 reader that dropped such a page and tried
/// the next slot read a whole region as never-joined (page 0 →
/// `manager_lease vacant`, no `seq_offset`, the leased slots' live roots
/// — which ride the page ONLY — gone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageRead {
    /// A verified page.
    Valid(AppenderPage),
    /// All zero — never written (a fresh pair, or a ring-side slot of an
    /// appender that never checkpointed).
    Blank,
    /// The magic and the checksum verify but byte 55 is not
    /// [`APPENDER_PAGE_LAYOUT_VERSION`]: a page another layout wrote.
    ForeignLayout { version: u8 },
    /// Anything else: torn or foreign bytes.
    Corrupt(String),
}

/// Whether a page image's magic and checksum verify (the layout byte is
/// not consulted) — what tells a page of another layout from a torn one.
fn page_checksum_verifies(buf: &[u8]) -> bool {
    buf.len() == APPENDER_PAGE_LEN
        && le32(buf, OFF_MAGIC) == APPENDER_PAGE_MAGIC
        && le64(buf, OFF_CHECKSUM) == page_checksum(buf)
}

/// Classify one image (total).
pub fn classify_page(buf: &[u8]) -> PageRead {
    if buf.iter().all(|b| *b == 0) {
        return PageRead::Blank;
    }
    if page_checksum_verifies(buf) && buf[OFF_LAYOUT_VERSION] != APPENDER_PAGE_LAYOUT_VERSION {
        return PageRead::ForeignLayout {
            version: buf[OFF_LAYOUT_VERSION],
        };
    }
    match AppenderPage::decode(buf) {
        Ok(p) => PageRead::Valid(p),
        Err(e) => PageRead::Corrupt(e.to_string()),
    }
}

/// The forward-only refusal a foreign-layout page slot produces.
fn foreign_layout_refusal(slot: usize, version: u8) -> KvError {
    KvError::Corrupt(format!(
        "appender page slot {slot} carries layout version {version} with a valid checksum — \
         this binary writes {APPENDER_PAGE_LAYOUT_VERSION} (PR 4: seq_offset at \
         {OFF_SEQ_OFFSET}, slot entries at {OFF_SLOTS}) and the format is forward-only: the \
         volume was written by a binary of another layout; reformat required (a page of the \
         PR 2/3 layout is REFUSED, never read as absent)"
    ))
}

/// Newest-valid-wins over an appender's page-slot images: the valid page
/// with the highest generation and its slot index; `Ok(None)` when no
/// image verifies (a never-joined appender, or every copy torn). A
/// [`PageRead::ForeignLayout`] in ANY slot REFUSES — never a fallback to
/// a predecessor (Issue 26).
pub fn newest_valid<B: AsRef<[u8]>>(
    images: &[B],
) -> Result<Option<(usize, AppenderPage)>, KvError> {
    let mut best: Option<(usize, AppenderPage)> = None;
    for (i, img) in images.iter().enumerate() {
        match classify_page(img.as_ref()) {
            PageRead::Valid(p) => {
                let newer = best
                    .as_ref()
                    .is_none_or(|(_, b)| p.generation > b.generation);
                if newer {
                    best = Some((i, p));
                }
            }
            PageRead::ForeignLayout { version } => return Err(foreign_layout_refusal(i, version)),
            PageRead::Blank | PageRead::Corrupt(_) => {}
        }
    }
    Ok(best)
}

// ---- Routing slot ↔ forest slot ------------------------------------------

/// The forest slot a page entry's routing `slot` names on a volume whose
/// native routing slot is `native`: the native slot is forest slot 0,
/// every other routing slot its guest keyspace (`slot + 1`).
pub fn forest_slot_of_page_slot(slot: u16, native: u16) -> super::record::ForestSlot {
    if slot == native {
        super::record::NATIVE_FOREST_SLOT
    } else {
        super::record::guest_forest_slot(slot)
    }
}

/// The inverse of [`forest_slot_of_page_slot`]. Refuses the guest
/// keyspace of the native slot itself — a volume's native slot is hosted
/// in its legacy keyspace, so that guest forest slot is never minted.
pub fn page_slot_of_forest_slot(
    forest: super::record::ForestSlot,
    native: u16,
) -> Result<u16, KvError> {
    if forest == super::record::NATIVE_FOREST_SLOT {
        return Ok(native);
    }
    if forest > super::record::FOREST_SLOT_MAX {
        return Err(KvError::Corrupt(format!(
            "forest slot {forest} is outside the slot namespace"
        )));
    }
    let slot = (forest - 1) as u16;
    if slot == native {
        return Err(KvError::Corrupt(format!(
            "forest slot {forest} is the guest keyspace of the native slot {native}, which is \
             never minted"
        )));
    }
    Ok(slot)
}

// ---- Device geometry ------------------------------------------------------

/// The four device offsets of appender 0's page slots inside the fixed
/// journal extent: `[A, B, R0, R1]` at its first four pages.
pub fn appender0_page_offsets(journal: &ExtentRef) -> [u64; APPENDER_PAGE_SLOTS] {
    let p = APPENDER_PAGE_LEN as u64;
    [
        journal.start,
        journal.start + p,
        journal.start + 2 * p,
        journal.start + 3 * p,
    ]
}

/// Appender 0's ring proper on a forest volume: the fixed journal extent
/// past its four reserved pages.
pub fn appender0_ring_extent(journal: &ExtentRef) -> ExtentRef {
    let reserved = APPENDER0_RESERVED_PAGES * APPENDER_PAGE_LEN as u64;
    ExtentRef {
        start: journal.start + reserved,
        len: journal.len.saturating_sub(reserved),
    }
}

/// Page pairs one directory extent of `node_size` bytes holds: every
/// page but the header page, in pairs.
pub fn dir_pairs_per_extent(node_size: u64) -> u64 {
    (node_size / APPENDER_PAGE_LEN as u64).saturating_sub(1) / 2
}

/// Device offset of the header page of a directory extent (its LAST page).
pub fn dir_header_offset(extent: &ExtentRef) -> u64 {
    extent.end() - APPENDER_PAGE_LEN as u64
}

/// Device offsets of pair `pair`'s `[A, B]` inside a directory extent.
pub fn dir_pair_offsets(extent: &ExtentRef, pair: u64) -> [u64; 2] {
    let a = extent.start + pair * 2 * APPENDER_PAGE_LEN as u64;
    [a, a + APPENDER_PAGE_LEN as u64]
}

/// Device offsets of an appender ≥ 1's ring-side pages `[R0, R1]`: the
/// first two pages of its ring's first segment.
pub fn ring_side_offsets(first_segment: &ExtentRef) -> [u64; 2] {
    [
        first_segment.start,
        first_segment.start + APPENDER_PAGE_LEN as u64,
    ]
}

/// The ring proper of an appender ≥ 1's FIRST segment: past its two
/// ring-side ledger pages.
pub fn first_segment_ring_part(first_segment: &ExtentRef) -> ExtentRef {
    let reserved = RING_SIDE_LEDGER_PAGES * APPENDER_PAGE_LEN as u64;
    ExtentRef {
        start: first_segment.start + reserved,
        len: first_segment.len.saturating_sub(reserved),
    }
}

/// The header page of one appender-directory extent (LE, checksummed):
/// `magic u32 ‖ chain_index u32 ‖ xxh3_64 ‖ next.start u64 ‖ next.len u64
/// ‖ pairs u16`, zero-padded to the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirHeader {
    /// This extent's index in the chain (0 = the one the superblock names).
    pub chain_index: u32,
    /// The next extent, or `len == 0` for the chain's end.
    pub next: ExtentRef,
    /// Page pairs this extent holds ([`dir_pairs_per_extent`]).
    pub pairs: u16,
}

const DIR_OFF_MAGIC: usize = 0;
const DIR_OFF_INDEX: usize = 4;
const DIR_OFF_CHECKSUM: usize = 8;
const DIR_OFF_NEXT: usize = 16;
const DIR_OFF_PAIRS: usize = 32;
const DIR_FIXED_LEN: usize = 34;

impl DirHeader {
    pub fn encode(&self) -> Vec<u8> {
        let mut img = vec![0u8; APPENDER_PAGE_LEN];
        img[DIR_OFF_MAGIC..DIR_OFF_MAGIC + 4].copy_from_slice(&APPENDER_DIR_MAGIC.to_le_bytes());
        img[DIR_OFF_INDEX..DIR_OFF_INDEX + 4].copy_from_slice(&self.chain_index.to_le_bytes());
        img[DIR_OFF_NEXT..DIR_OFF_NEXT + 8].copy_from_slice(&self.next.start.to_le_bytes());
        img[DIR_OFF_NEXT + 8..DIR_OFF_NEXT + 16].copy_from_slice(&self.next.len.to_le_bytes());
        img[DIR_OFF_PAIRS..DIR_OFF_PAIRS + 2].copy_from_slice(&self.pairs.to_le_bytes());
        let sum = dir_checksum(&img);
        img[DIR_OFF_CHECKSUM..DIR_OFF_CHECKSUM + 8].copy_from_slice(&sum.to_le_bytes());
        img
    }

    pub fn decode(buf: &[u8]) -> Result<Self, KvError> {
        if buf.len() != APPENDER_PAGE_LEN {
            return Err(KvError::Corrupt(format!(
                "appender directory header must be {APPENDER_PAGE_LEN} bytes, got {}",
                buf.len()
            )));
        }
        if le32(buf, DIR_OFF_MAGIC) != APPENDER_DIR_MAGIC {
            return Err(KvError::Corrupt(
                "bad appender directory header magic".to_string(),
            ));
        }
        let stored = le64(buf, DIR_OFF_CHECKSUM);
        let computed = dir_checksum(buf);
        if stored != computed {
            return Err(KvError::ChecksumMismatch { stored, computed });
        }
        if buf[DIR_FIXED_LEN..].iter().any(|b| *b != 0) {
            return Err(KvError::Corrupt(
                "appender directory header padding is not zero".to_string(),
            ));
        }
        Ok(Self {
            chain_index: le32(buf, DIR_OFF_INDEX),
            next: ExtentRef {
                start: le64(buf, DIR_OFF_NEXT),
                len: le64(buf, DIR_OFF_NEXT + 8),
            },
            pairs: le16(buf, DIR_OFF_PAIRS),
        })
    }
}

fn dir_checksum(img: &[u8]) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    h.update(&img[..DIR_OFF_CHECKSUM]);
    h.update(&[0u8; 8]);
    h.update(&img[DIR_OFF_CHECKSUM + 8..]);
    h.digest()
}

// ---- Derivations ----------------------------------------------------------

/// The per-appender ring FLOOR, bytes: the §4.4 pt 5 checkpoint carve-out
/// (`max(256 KiB, ring/64)` = 256 KiB at this size) plus one
/// [`super::journal::MAX_ENTRY_LEN`] (128 KiB) must be admissible or a
/// legal transaction could never commit — 384 KiB — rounded UP to the
/// power of two so the segment count is a whole number of the shipped
/// 64 KiB–1 MiB extents: 512 KiB.
pub const SYM_RING_FLOOR_BYTES: u64 =
    (256 * 1024 + super::journal::MAX_ENTRY_LEN).next_power_of_two();

/// The per-appender ring CEILING for a volume of `volume_len` bytes: the
/// solo ring's own derivation (`clamp(volume/64, 8 MiB, 32 MiB)`) — an
/// appender never needs more than the whole volume did.
pub fn sym_ring_ceiling_bytes(volume_len: u64) -> u64 {
    super::superblock::journal_ring_len(volume_len).max(SYM_RING_FLOOR_BYTES)
}

/// The DERIVED per-appender ring size (§1.6 "Per-appender ring"):
/// `clamp(2 × ewma_commit_bytes_per_s × CHECKPOINT_MAX_AGE, floor,
/// ceiling)` — two checkpoint ages of the appender's measured commit
/// stream (`2 ×` = one EWMA window of under-estimate), never below the
/// admissibility floor, never above the solo ring. A fresh join has no
/// EWMA and takes the floor; growth on `journal_full_stalls` is what
/// carries a busy appender toward the ceiling.
pub fn appender_ring_bytes_derived(ewma_commit_bytes_per_s: u64, volume_len: u64) -> u64 {
    let age_ms = super::checkpoint::CHECKPOINT_MAX_AGE_MS as u64;
    let want = ewma_commit_bytes_per_s
        .saturating_mul(2)
        .saturating_mul(age_ms)
        / 1000;
    let ceiling = sym_ring_ceiling_bytes(volume_len);
    let bytes = want.clamp(SYM_RING_FLOOR_BYTES, ceiling);
    bytes / APPENDER_PAGE_LEN as u64 * APPENDER_PAGE_LEN as u64
}

/// `SQUEEZEFS_SYM_RING_KB` — the explicit per-appender ring size, KiB
/// (range floor..=ceiling in KiB; explicit wins verbatim over the
/// derivation, ENG-10).
pub const SYM_RING_KB_ENV: &str = "SQUEEZEFS_SYM_RING_KB";

/// The ring size an appender ≥ 1 joins with: the knob when set, else the
/// derivation at `ewma_commit_bytes_per_s`. A knob above the volume's
/// ceiling is clamped to it, logged once (the registry range is the
/// process-wide bound; the ceiling is per volume).
pub fn resolve_sym_ring_bytes(ewma_commit_bytes_per_s: u64, volume_len: u64) -> u64 {
    match crate::env_knobs::opt_int_knob::<u64>(SYM_RING_KB_ENV) {
        Some(kib) => {
            let want = kib.saturating_mul(1024);
            let ceiling = sym_ring_ceiling_bytes(volume_len);
            if want > ceiling {
                log::warn!(
                    "{SYM_RING_KB_ENV}={kib} KiB exceeds this volume's per-appender ring \
                     ceiling ({ceiling} B = the solo ring's derivation) — clamped"
                );
            }
            want.clamp(SYM_RING_FLOOR_BYTES, ceiling)
        }
        None => appender_ring_bytes_derived(ewma_commit_bytes_per_s, volume_len),
    }
}

/// [`resolve_sym_ring_bytes`] for a REJOINING identity (PR 13g, F-R5):
/// the knob verbatim when set (explicit wins), else the larger of the
/// fresh derivation and the identity's durable `appender_hint` (the size its
/// predecessor incarnation grew to under load — PR 3's owed "persisted
/// commit-rate EWMA as the ring-size input"), clamped to the volume's
/// floor / ceiling and page-aligned. A hint of 0 is no hint.
pub fn resolve_sym_ring_bytes_hinted(hint_bytes: u64, volume_len: u64) -> u64 {
    if crate::env_knobs::opt_int_knob::<u64>(SYM_RING_KB_ENV).is_some() {
        return resolve_sym_ring_bytes(0, volume_len);
    }
    let ceiling = sym_ring_ceiling_bytes(volume_len);
    let bytes = appender_ring_bytes_derived(0, volume_len)
        .max(hint_bytes)
        .clamp(SYM_RING_FLOOR_BYTES, ceiling);
    bytes / APPENDER_PAGE_LEN as u64 * APPENDER_PAGE_LEN as u64
}

/// The ring budget of a volume with a heap of `heap_len` bytes:
/// `heap / 16` (§1.6 "Ring budget per volume") — four times the solo
/// ring's `volume/64` share, leaving a quarter of the heap for images at
/// the peak.
pub fn ring_budget_bytes(heap_len: u64) -> u64 {
    heap_len / 16
}

/// `appenders_capacity`: how many appenders the ring budget admits at
/// `ring_bytes` each — the ONE hard resource a join refuses on (§5.11).
pub fn appenders_capacity(heap_len: u64, ring_bytes: u64) -> u64 {
    if ring_bytes == 0 {
        return 0;
    }
    ring_budget_bytes(heap_len) / ring_bytes
}

/// The ring budget's REMAINDER: `heap/16` less the ring bytes every
/// `Live` page names (`appender_ring_budget_remaining_bytes`; PR 13g
/// review round 1, Issue 8) — what a `GrowRing` may still carve.
pub fn ring_budget_remaining_bytes(heap_len: u64, rings_in_use: u64) -> u64 {
    ring_budget_bytes(heap_len).saturating_sub(rings_in_use)
}

/// **A join's ring under the ring budget** (PR 13g review round 2, Issue
/// 18): the join is ADMITTED by the count law (`appenders_capacity` =
/// `heap/16 ÷ floor` — every admitted appender is budgeted one FLOOR
/// ring), and its ring is `ask` only as far as the budget's remainder
/// holds it: `min(ask, remainder)` in whole extents (the ring is carved
/// by the node), never below the floor the count law budgets —
/// `grow_ring_room_bytes`'s law at the join. A hinted or explicit ask
/// above the remainder lands the remainder and grows later (`GrowRing`,
/// under the same budget).
pub fn join_ring_bytes_under_budget(ask_bytes: u64, budget_remaining: u64, node_size: u64) -> u64 {
    let node = node_size.max(1);
    ask_bytes
        .min(budget_remaining / node * node)
        .max(SYM_RING_FLOOR_BYTES)
}

/// **`GrowRing`'s room** (Issue 8): the smaller of the per-appender
/// ceiling less the ring the page names and the set-wide budget's
/// remainder — N rings grown toward the ceiling never exceed `heap/16`,
/// the image heap the budget reserves.
pub fn grow_ring_room_bytes(ceiling: u64, ring_bytes: u64, budget_remaining: u64) -> u64 {
    ceiling.saturating_sub(ring_bytes).min(budget_remaining)
}

/// **A `GrowRing` carve's SEGMENT FLOOR** (PR 13g review round 1, Issue
/// 9): the page's table holds `RING_SEGMENTS_MAX` segments, so a slot is
/// spent only on a run of the DOUBLING class — at least half the clamped
/// ask (`want_extents` after the room). A fragmented heap whose longest
/// adjacent run is shorter answers `None` with every claim released: the
/// ring stays at its size (pressure cycles keep it drained) and the next
/// ask retries on a heap the cadence's returns may have healed. The
/// floor is one extent when the ask itself is one.
pub fn grow_ring_segment_floor_extents(want_extents: u64) -> u64 {
    want_extents.div_ceil(2).max(1)
}

// ---- Extent grants (§5.3.3) -------------------------------------------------

/// The images ONE SMO of a slot tree claims at most (physical): a
/// compaction claims 1; a leaf split claims its parts — a one-node log
/// folds into ≤ 2 parts at the ¾ fill, and the frozen delta riding the
/// split can add a third (`NodeLayout::smo_extents_for_parts`'s `parts +
/// 2` over-promise is the ADMISSION's cushion, not a claim count) — plus
/// the new root a ROOT leaf's split mints. The flush pass's REACTIVE
/// refill of an exhausted grant asks for exactly this from the
/// compaction reserve (review round 2, Issue 18): the manager's own SMOs
/// draw the reserve one image at a time, and a refill that took the full
/// derived grant (the floor 8, or more) into one region's grant would
/// starve the manager's recovery-class claims on a near-full heap.
pub const SMO_IMAGES_MAX: u32 = 3 + 1;
/// The extent-grant FLOOR, extents: one image per pending root swap per
/// checkpoint cycle (a compaction or split of a slot tree claims ≤ 2
/// extents) × up to 4 pending swaps per cycle — the mixed tree's SMO
/// budget (physical, §5.3.3).
pub const GRANT_EXTENTS_FLOOR: u64 = 2 * 4;
/// The registry ceiling of `SQUEEZEFS_SYM_GRANT_EXTENTS`: the u16 slot
/// namespace's width — more extents than a volume has slot trees to
/// compact is a units mistake.
pub const GRANT_EXTENTS_MAX: u64 = 65_536;

/// `SQUEEZEFS_SYM_GRANT_EXTENTS` — the explicit extent-grant size
/// (int floor..=65536; explicit wins verbatim over the derivation).
pub const SYM_GRANT_EXTENTS_ENV: &str = "SQUEEZEFS_SYM_GRANT_EXTENTS";

/// The DERIVED grant size (§5.3.3): `clamp(2 × ewma_smo_rate ×
/// manager_failover_bound_s, floor, free_heap / (4 × appenders))` — the
/// headroom an appender needs to keep compacting through a manager
/// failover at its measured SMO rate (`2 ×` = one EWMA window of
/// under-estimate), never below the SMO budget floor, never more than a
/// quarter of the free heap spread over the appenders. `ewma_smo_milli`
/// is the appender's SMO rate in milli-SMOs per second.
pub fn grant_extents_derived(
    ewma_smo_milli_per_s: u64,
    failover_bound_ms: u64,
    free_heap: u64,
    appenders: u64,
) -> u64 {
    // milli-SMO/s × ms = micro-SMOs; 2 × … / 1_000_000 = SMOs over the
    // bound, doubled.
    let want = ewma_smo_milli_per_s
        .saturating_mul(2)
        .saturating_mul(failover_bound_ms)
        / 1_000_000;
    // The third term IS the wire cap (one definition — Issue 7).
    let cap = grant_extents_wire_cap(free_heap, appenders);
    want.clamp(GRANT_EXTENTS_FLOOR, cap)
}

/// The grant size in force: the knob verbatim, else the derivation.
pub fn resolve_grant_extents(
    ewma_smo_milli_per_s: u64,
    failover_bound_ms: u64,
    free_heap: u64,
    appenders: u64,
) -> u64 {
    match crate::env_knobs::opt_int_knob::<u64>(SYM_GRANT_EXTENTS_ENV) {
        Some(n) => n,
        None => grant_extents_derived(
            ewma_smo_milli_per_s,
            failover_bound_ms,
            free_heap,
            appenders,
        ),
    }
}

/// `manager_failover_bound_ms` (§1.6 "Manager death — FOREIGN-homed
/// appenders", §5.9): the writer-claim TTL the D0 ladder waits out
/// (`CLIENT_STALE_TTL_SECS`) + the ladder's own wall (flock absorption +
/// the PR round trips, measured at this open) + the ring replay's wall
/// (measured at this open) — DERIVED from the constants and the two
/// measured terms, published live, and the term the grant headroom is
/// sized against.
pub fn manager_failover_bound_ms(stale_ttl_secs: u64, ladder_ms: u64, replay_ms: u64) -> u64 {
    stale_ttl_secs
        .saturating_mul(1000)
        .saturating_add(ladder_ms)
        .saturating_add(replay_ms)
}

/// The §5.5.2 vol-0 rule: a manager that cannot read volume 0's ledger for
/// longer than `T_owner` RELEASES its manager role rather than act on a
/// stale death ledger. The decision, as one function — first product
/// caller: PR 8/10's role driver (the gauge `manager_vol0_unreachable`
/// counts the releases); the derivation tie test is its consumer today.
pub fn manager_should_release_role(unreachable_for_ms: u64, t_owner_ms: u64) -> bool {
    unreachable_for_ms > t_owner_ms
}

/// The service-edge validation of a wire `ReturnExtents` (review round 1,
/// Issue 2 — "bounded codec = bounded execution"): every run must lie
/// inside the volume's `total_extents` and `start + len` must not
/// overflow; the first offending run is the rejection. Pure over the
/// integers: nothing here allocates (fuzzed by `manager_call_frame`,
/// mirrored in `decoder_property_tests`).
pub fn validate_return_runs(runs: &[GrantRun], total_extents: u64) -> Result<(), GrantRun> {
    for r in runs {
        match r.start.checked_add(u64::from(r.len)) {
            Some(end) if end <= total_extents => {}
            _ => return Err(*r),
        }
    }
    Ok(())
}

/// The frame's runs COALESCED: sorted by start, overlapping and adjacent
/// runs merged, an overflowing or empty run dropped — a list of DISJOINT
/// ascending runs no longer than the input (one pass over the frame's own
/// runs, O(n log n), the only allocation proportional to the frame — and
/// bounded by the frame's own byte length, never by the integers it
/// carries). Review round 2, Issue 2 residual: the intersection below
/// used to emit one extent per (frame run × record extent) BEFORE a
/// dedup, so 250K copies of one run over a 4,096-extent record built an
/// 8 GiB list on the manager; coalescing first makes every later step
/// bounded by the record.
pub fn coalesce_runs(runs: &[GrantRun]) -> Vec<GrantRun> {
    let mut sorted: Vec<GrantRun> = runs
        .iter()
        .filter(|r| r.len > 0 && r.start.checked_add(u64::from(r.len)).is_some())
        .copied()
        .collect();
    sorted.sort_unstable_by_key(|r| (r.start, r.len));
    let mut out: Vec<GrantRun> = Vec::new();
    for r in sorted {
        let r_end = r.start + u64::from(r.len);
        match out.last_mut() {
            Some(last) if r.start <= last.start + u64::from(last.len) => {
                let last_end = last.start + u64::from(last.len);
                let end = last_end.max(r_end);
                // A merged run longer than the wire's `u32` splits into
                // `u32::MAX`-long pieces — every piece, not one (review
                // round 3, Issue 20): the pieces are adjacent, so the
                // output stays a list of ascending runs the merge walk
                // below reads correctly. Unreachable behind
                // `validate_return_runs` (every run lies inside the
                // volume; > 2^32 extents is a 1 EiB metadata volume), so
                // it is the codec's own bound made exact, not a path.
                let mut piece_start = last.start;
                let mut remaining = end - last.start;
                last.len = u32::try_from(remaining).unwrap_or(u32::MAX);
                remaining -= u64::from(last.len);
                piece_start += u64::from(last.len);
                while remaining > 0 {
                    let len = u32::try_from(remaining).unwrap_or(u32::MAX);
                    out.push(GrantRun {
                        start: piece_start,
                        len,
                    });
                    remaining -= u64::from(len);
                    piece_start += u64::from(len);
                }
            }
            _ => out.push(r),
        }
    }
    debug_assert!(
        out.windows(2)
            .all(|w| w[0].start + u64::from(w[0].len) <= w[1].start),
        "coalesced runs are ascending and non-overlapping"
    );
    out
}

/// Extents a list of runs names (saturating).
pub fn runs_extent_count(runs: &[GrantRun]) -> u64 {
    runs.iter()
        .fold(0u64, |n, r| n.saturating_add(u64::from(r.len)))
}

/// The extents `coalesced` — the frame's runs after [`coalesce_runs`]:
/// disjoint (or adjacent), ascending — name INSIDE `record`, as a
/// strictly ascending list: a merge walk over the two ascending lists,
/// so every emitted extent is distinct by construction (no dedup, no
/// intermediate list) and the output is bounded by the RECORD's extent
/// count — the return verb's only allocations are the coalesced runs (≤
/// the frame's) and this output (≤ the record's).
pub fn intersect_coalesced_with_record(
    coalesced: &[GrantRun],
    record: &super::slot_state::ExtentGrantRecord,
) -> Vec<u64> {
    let mut extents: Vec<u64> = Vec::new();
    // Both lists are ascending and non-overlapping: a merge walk emits
    // each intersection once, in order.
    let (mut i, mut j) = (0usize, 0usize);
    while i < coalesced.len() && j < record.runs.len() {
        let r = coalesced[i];
        let g = record.runs[j];
        let (rs, re) = (r.start, r.start + u64::from(r.len));
        let (gs, ge) = (g.start, g.start + u64::from(g.len));
        let (s, e) = (rs.max(gs), re.min(ge));
        if s < e {
            extents.extend(s..e);
        }
        if re < ge {
            i += 1;
        } else {
            j += 1;
        }
    }
    extents
}

/// The extents a wire `ExtentGrant { want }` may CARVE: `want == 0` is the
/// derived size `cap`; an explicit want is clamped to it — a wire integer
/// is never an allocation authority.
pub fn clamp_grant_want(want: u32, cap: u64) -> u64 {
    if want == 0 {
        cap
    } else {
        u64::from(want).min(cap)
    }
}

/// The heap-share cap a WIRE appender's explicit `ExtentGrant { want }`
/// is clamped to (PR 13g, F-R5): the derivation's own third term,
/// `free_heap / (4 × appenders)` floored at [`GRANT_EXTENTS_FLOOR`] — a
/// quarter of the free heap spread over the live appenders. The manager
/// measures no SMO rate for an appender it does not flush (the rate is
/// the appender's, folded on ITS flush pass), so its own derivation for
/// one reads a rate of 0 and the floor, and a joiner under a storm
/// could never be answered more than 8 extents however it asked — the
/// grant / return ping-pong at the one-SMO grain. The joiner derives its
/// size off its measured rate and asks it; the manager bounds the ask by
/// what durable state allows the derivation to reach — a wire integer is
/// still never an allocation authority.
pub fn grant_extents_wire_cap(free_heap: u64, appenders: u64) -> u64 {
    (free_heap / (4 * appenders.max(1))).max(GRANT_EXTENTS_FLOOR)
}

/// The framed bytes of one allocator CLAIM delta inside a control entry
/// (`alloc_ext::alloc_record` — an 8-byte extent key, a 1-byte value).
pub fn alloc_delta_frame_len() -> u64 {
    super::journal::record_frame_len(super::alloc_ext::EXTENT_KEY_LEN, 1)
}

/// The framed bytes of one allocator FREE delta inside a control entry
/// (`alloc_ext::free_record` — the tag and the retire seq).
pub fn free_delta_frame_len() -> u64 {
    super::journal::record_frame_len(super::alloc_ext::EXTENT_KEY_LEN, 9)
}

/// The framed bytes of an `appender_hint` put riding a grant's entry.
pub fn appender_hint_frame_len() -> u64 {
    super::journal::record_frame_len(
        super::slot_state::APPENDER_HINT_KEY_LEN,
        super::slot_state::APPENDER_HINT_LEN,
    )
}

/// **The control entries a grant's deltas ride** (PR 13g review round 1,
/// Issue 3 — PR 4's leave law for the extent grant): `count` deltas of
/// `delta_frame_len` bytes each, packed by [`super::journal::pack_entries`]
/// under [`super::journal::MAX_ENTRY_LEN`] BESIDE the rewritten
/// `extent_grant` record and `side_frame_len` bytes of other riders (the
/// identity's hint put). Chunk `i`'s entry carries the record AS IT
/// STANDS AFTER the chunk — `record ∪ deltas[..range.end]` — so its frame
/// is bounded by `record_runs + range.end` runs, the CUMULATIVE count
/// (every extent adds at most one run: a claim that touches no run, a
/// free that splits one; review round 2, Issue 15 — a budget of `range.
/// len()` under-counted every chunk past the first by `range.start` runs
/// and refused chunk 2 of a fragmented carve `EntryTooLarge`) and capped
/// at `record_runs_cap` (the tree-0 value cap's `extent_grant_max_runs` —
/// the record can never carry more, the carve is cut and the return
/// refused at it), so the packing is exact in O(count) and never a
/// materialized record per candidate. Before the chunking a carve past
/// ≈ 5,200 extents or a return past ≈ 3,900 was `EntryTooLarge` for ever:
/// the joiner's derived ask at a storm's SMO rate on a heap with room.
pub fn pack_grant_deltas(
    count: usize,
    delta_frame_len: u64,
    record_runs: usize,
    record_runs_cap: usize,
    side_frame_len: u64,
) -> Vec<std::ops::Range<usize>> {
    let payloads = vec![delta_frame_len; count];
    super::journal::pack_entries(&payloads, |range| {
        super::slot_state::extent_grant_frame_len((record_runs + range.end).min(record_runs_cap))
            .saturating_add(side_frame_len)
    })
}

/// The deltas ONE control entry carries beside a grant record of
/// `record_runs` runs (capped at `record_runs_cap`) and `side_frame_len`
/// bytes of riders — the first chunk [`pack_grant_deltas`] cuts, in
/// closed form: `(payload cap − record frame(record_runs) − side) /
/// (delta frame + one run)` while the record can still grow a run per
/// delta, else `(payload cap − record frame(cap) − side) / delta frame`
/// once it sits at the cap. The derivation's face for the tie test and
/// the operator's arithmetic.
pub fn grant_deltas_per_entry(
    delta_frame_len: u64,
    record_runs: usize,
    record_runs_cap: usize,
    side_frame_len: u64,
) -> u64 {
    let payload = super::journal::entry_payload_cap();
    let growing = {
        let fixed =
            super::slot_state::extent_grant_frame_len(record_runs).saturating_add(side_frame_len);
        payload.saturating_sub(fixed)
            / delta_frame_len.saturating_add(super::slot_state::GRANT_RUN_LEN as u64)
    };
    if record_runs.saturating_add(growing as usize) <= record_runs_cap {
        return growing;
    }
    let fixed =
        super::slot_state::extent_grant_frame_len(record_runs_cap).saturating_add(side_frame_len);
    payload.saturating_sub(fixed) / delta_frame_len.max(1)
}

/// **The ring segments a join KEEPS of a fragmented carve** (PR 13g review
/// round 1, Issue 4): `runs` are the carve's claims coalesced into runs
/// (ascending by start, in EXTENTS); `floor_extents` the ring floor. Every
/// run when they fit the page's table (`RING_SEGMENTS_MAX`); else the
/// LARGEST runs that fit HALF the table — a sized join leaves growth its
/// room — when their total reaches the floor, else the largest that fit
/// the whole table (a heap so fragmented that eight runs are under the
/// floor still joins at what fits: the floor's extents are at most eight
/// at any node size the format admits, so eight one-extent runs are the
/// floor). Answers `(kept, released)`, both ascending by start — the
/// released extents go back to the heap, the joiner grows later
/// (`GrowRing`'s law). Before it the join REFUSED `Corrupt` naming a knob
/// the operator never set, on the crash-rejoin path the hint exists for.
pub fn ring_segments_that_fit(runs: &[GrantRun], floor_extents: u64) -> (Vec<GrantRun>, Vec<u64>) {
    if runs.len() <= RING_SEGMENTS_MAX {
        return (runs.to_vec(), Vec::new());
    }
    let mut by_len: Vec<GrantRun> = runs.to_vec();
    by_len.sort_by(|a, b| b.len.cmp(&a.len).then(a.start.cmp(&b.start)));
    let total = |n: usize| -> u64 { by_len.iter().take(n).map(|r| u64::from(r.len)).sum() };
    let half = RING_SEGMENTS_MAX / 2;
    let keep_n = if total(half) >= floor_extents {
        half
    } else {
        RING_SEGMENTS_MAX
    };
    let (mut kept, dropped) = {
        let (k, d) = by_len.split_at(keep_n);
        (k.to_vec(), d.to_vec())
    };
    kept.sort_by_key(|r| r.start);
    let mut released: Vec<u64> = dropped
        .iter()
        .flat_map(|r| r.start..r.start + u64::from(r.len))
        .collect();
    released.sort_unstable();
    (kept, released)
}

/// **The joined appender's POOL TARGET** (PR 13g review round 1, Issue 5
/// — ONE size for the recycle's keep, the shrink's mark and the ask):
/// `max(derived + promised, pool_floor)` bounded by `cap` (the heap-share
/// cap a wire ask is clamped to — an ask above it is answered verbatim)
/// and never below `promised` (the headroom the admitted SMOs already
/// hold). `pool_floor` is the JOIN's cost class — `GRANT_EXTENTS_FLOOR +
/// M` (the rotor the joiner mints lazily, one image each, none of them a
/// promise): a quiet joiner's pool settles at the size its join carved,
/// never at the derived floor that would make its first storm cycle ask
/// one SMO at a time again, and never at the last storm's size for its
/// lifetime.
pub fn joined_pool_target(derived: u64, promised: u64, pool_floor: u64, cap: u64) -> u64 {
    derived
        .saturating_add(promised)
        .max(pool_floor)
        .min(cap)
        .max(promised)
}

/// The join's cost class — the standing pool a quiet joiner keeps.
pub fn joined_pool_floor(mint_slots: u64) -> u64 {
    GRANT_EXTENTS_FLOOR + mint_slots
}

/// **The joined appender's refill law** (Issue 5b): an ask is due when the
/// HEADROOM (unclaimed less promised) fell below half the derived size —
/// the 50 % law over what the pool can still promise; the promises
/// themselves are inside the target the ask names.
pub fn joined_refill_due(headroom: u64, derived: u64) -> bool {
    headroom < derived / 2
}

/// The ring bytes every `Live` page of a directory names — the budget's
/// consumers (`ring_budget_remaining_bytes`'s input).
pub fn rings_in_use(entries: &[AppenderEntry]) -> u64 {
    entries
        .iter()
        .filter_map(|e| e.page.as_ref())
        .filter(|p| p.state == AppenderState::Live)
        .map(|p| p.ring_bytes())
        .sum()
}

/// The runs a page NAMES of a remainder `runs` (ascending by start):
/// every run when they fit the page's [`GRANT_RUNS_MAX`], else the
/// LARGEST runs (ties: the lowest start), back in ascending order.
pub fn page_runs_of(runs: &[GrantRun]) -> Vec<GrantRun> {
    if runs.len() <= GRANT_RUNS_MAX {
        return runs.to_vec();
    }
    let mut by_len: Vec<GrantRun> = runs.to_vec();
    by_len.sort_by(|a, b| b.len.cmp(&a.len).then(a.start.cmp(&b.start)));
    by_len.truncate(GRANT_RUNS_MAX);
    by_len.sort_by_key(|r| r.start);
    by_len
}

/// The highest record seq any ring of this volume can have stamped: half
/// the `u64` space. A seq is a ring POSITION plus an offset, positions are
/// journal bytes, and every offset raise sets a frontier at most one above
/// another ring's frontier — so the seq space grows by the bytes journaled
/// plus one per grant, and 2⁶³ journal bytes is provably not a frontier
/// (review round 6, Issue 29: a frontier past it would saturate every
/// later stamp to `u64::MAX`, collapsing the leaf fold's per-key LWW to
/// insertion order and making the next replay skip every window record as
/// "already materialized").
pub const SEQ_FRONTIER_SANE_MAX: u64 = u64::MAX / 2;

/// The highest record-seq frontier a WIRE appender can legitimately
/// present as a release's `seq_floor`, derived from DURABLE state the
/// manager reads (review round 6, Issue 29 — PR 3's bounded-execution law
/// for the slot words): its page's `seq_offset` — or the highest floor
/// this manager granted it plus one, when that grant's raise is not yet on
/// its page — plus its page's `head_hint`, plus TWICE its ring's length.
/// The newest durable page is at most one checkpoint stale (the in-flight
/// write), a ring's head advances at most its length past the tail the
/// page declared before another checkpoint writes the page again, so two
/// lengths bound the head at any instant. Capped at
/// [`SEQ_FRONTIER_SANE_MAX`], so a corrupt page cannot admit an insane
/// floor either.
pub fn release_seq_floor_bound(
    page_seq_offset: u64,
    granted_floor_max: Option<u64>,
    head_hint: u64,
    ring_len: u64,
) -> u64 {
    let offset = page_seq_offset.max(granted_floor_max.map_or(0, |f| f.saturating_add(1)));
    offset
        .saturating_add(head_hint)
        .saturating_add(ring_len.saturating_mul(2))
        .min(SEQ_FRONTIER_SANE_MAX)
}

/// The DURABLE / derived state a wire release's slot words are screened
/// against ([`screen_release_words`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseWordBounds {
    /// The grant-time floor (the lease's recorded `seq_floor`): the
    /// lessee's offset raise put its next stamp at `floor + 1` at the
    /// least, so a legitimate frontier is STRICTLY above it — and a floor
    /// at or below it would leave ring 0 stamping below the departing
    /// ring's records (the Issue-28 find's shape).
    pub seq_floor_recorded: u64,
    /// [`release_seq_floor_bound`]'s value: the highest frontier the
    /// departing ring can have reached.
    pub seq_floor_max: u64,
    /// The grant-time cursor: a mint cursor is never lowered (§5.1.8 — a
    /// remount installs tree 0's value as the slot's floor, so a lower
    /// one would re-mint live inos).
    pub cursor_recorded: u64,
    /// The slot's local-ino ceiling (`GUEST_NS_BASE`: a guest local is 40
    /// bits — the routing namespace's own bound).
    pub cursor_max: u64,
    /// The grant-time root — accepted verbatim (a lessee that moved
    /// nothing).
    pub root_recorded: (u64, u64),
    /// The heap's first byte and the node size: a root is a node-aligned
    /// heap address.
    pub heap_base: u64,
    pub node_size: u64,
    /// The volume's extent count: a root's extent lies inside it, and
    /// `slot_tree_extents` never exceeds it.
    pub total_extents: u64,
}

/// Why a wire release's slot words are REJECTED (`manager_verb_rejected`
/// — the buggy/hostile-peer class, kept off the must-stay-0 witness
/// gauge).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseWordRefusal {
    /// `seq_floor` at or below the grant's floor.
    SeqFloorBelowGrant { floor: u64, recorded: u64 },
    /// `seq_floor` above what the departing ring can have stamped.
    SeqFloorAboveBound { floor: u64, bound: u64 },
    /// `cursor` below the grant's cursor.
    CursorBelowGrant { cursor: u64, recorded: u64 },
    /// `cursor` past the slot's local-ino namespace.
    CursorAboveNamespace { cursor: u64, max: u64 },
    /// `slot_tree_extents` past the volume.
    ExtentsAboveVolume { extents: u32, total: u64 },
    /// `root` neither the recorded root nor a node-aligned heap address
    /// whose extent the appender's grant record holds.
    RootOutsideGrant { addr: u64 },
}

impl std::fmt::Display for ReleaseWordRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SeqFloorBelowGrant { floor, recorded } => write!(
                f,
                "seq_floor {floor} is not above the grant's floor {recorded}"
            ),
            Self::SeqFloorAboveBound { floor, bound } => write!(
                f,
                "seq_floor {floor} is above the departing ring's derived frontier bound {bound}"
            ),
            Self::CursorBelowGrant { cursor, recorded } => {
                write!(f, "cursor {cursor} is below the grant's cursor {recorded}")
            }
            Self::CursorAboveNamespace { cursor, max } => {
                write!(
                    f,
                    "cursor {cursor} is past the slot's local-ino namespace {max}"
                )
            }
            Self::ExtentsAboveVolume { extents, total } => write!(
                f,
                "slot_tree_extents {extents} exceeds the volume's {total} extents"
            ),
            Self::RootOutsideGrant { addr } => write!(
                f,
                "root {addr:#x} is neither the grant's root nor a node inside the appender's \
                 extent grant"
            ),
        }
    }
}

/// The extent index of a node-aligned heap address under `bounds`, or
/// `None` for an address below the heap, off the node grid, or past the
/// volume.
pub fn root_extent_of(addr: u64, bounds: &ReleaseWordBounds) -> Option<u64> {
    if bounds.node_size == 0 || addr < bounds.heap_base {
        return None;
    }
    let off = addr - bounds.heap_base;
    if !off.is_multiple_of(bounds.node_size) {
        return None;
    }
    let extent = off / bounds.node_size;
    (extent < bounds.total_extents).then_some(extent)
}

/// Screen a wire release's slot words against the durable / derived
/// bounds BEFORE any RAM or durable effect (review round 6, Issue 29):
/// `seq_floor` strictly above the grant's floor and at most the derived
/// frontier bound; `cursor` at least the grant's and inside the slot's
/// namespace; `slot_tree_extents` inside the volume; `root` the recorded
/// one verbatim, or a node-aligned heap address whose extent `grant`
/// holds (the header at that address is the caller's second witness —
/// I/O, so not here). Pure over the integers, allocation-free; fuzzed by
/// `manager_call_frame`, mirrored in `decoder_property_tests`.
pub fn screen_release_words(
    words: &crate::slot_lease_core::SlotWords,
    bounds: &ReleaseWordBounds,
    grant: &super::slot_state::ExtentGrantRecord,
) -> Result<(), ReleaseWordRefusal> {
    if words.seq_floor <= bounds.seq_floor_recorded {
        return Err(ReleaseWordRefusal::SeqFloorBelowGrant {
            floor: words.seq_floor,
            recorded: bounds.seq_floor_recorded,
        });
    }
    if words.seq_floor > bounds.seq_floor_max {
        return Err(ReleaseWordRefusal::SeqFloorAboveBound {
            floor: words.seq_floor,
            bound: bounds.seq_floor_max,
        });
    }
    if words.cursor < bounds.cursor_recorded {
        return Err(ReleaseWordRefusal::CursorBelowGrant {
            cursor: words.cursor,
            recorded: bounds.cursor_recorded,
        });
    }
    if words.cursor > bounds.cursor_max {
        return Err(ReleaseWordRefusal::CursorAboveNamespace {
            cursor: words.cursor,
            max: bounds.cursor_max,
        });
    }
    if u64::from(words.extents) > bounds.total_extents {
        return Err(ReleaseWordRefusal::ExtentsAboveVolume {
            extents: words.extents,
            total: bounds.total_extents,
        });
    }
    if words.root != bounds.root_recorded
        && !root_extent_of(words.root.0, bounds).is_some_and(|e| grant.contains(e))
    {
        return Err(ReleaseWordRefusal::RootOutsideGrant { addr: words.root.0 });
    }
    Ok(())
}

/// Screen one word of a wire `PublishRoots` (PR 12b round 4 — the
/// overflow law's wire form) against the same bounds a release's words
/// meet, MINUS the seq-floor clauses: a publication moves no floor (the
/// lease's stays), only the root, the cursor and the extent count.
/// `cursor` at least the grant's and inside the slot's namespace;
/// `slot_tree_extents` inside the volume; `root` the recorded one
/// verbatim, or a node-aligned heap address whose extent `grant` holds
/// (the node at that address is the caller's second witness — I/O, so
/// not here). Pure, allocation-free; fuzzed by `manager_call_frame`,
/// mirrored in `decoder_property_tests`.
pub fn screen_publish_root_words(
    words: &crate::slot_lease_core::SlotWords,
    bounds: &ReleaseWordBounds,
    grant: &super::slot_state::ExtentGrantRecord,
) -> Result<(), ReleaseWordRefusal> {
    if words.cursor < bounds.cursor_recorded {
        return Err(ReleaseWordRefusal::CursorBelowGrant {
            cursor: words.cursor,
            recorded: bounds.cursor_recorded,
        });
    }
    if words.cursor > bounds.cursor_max {
        return Err(ReleaseWordRefusal::CursorAboveNamespace {
            cursor: words.cursor,
            max: bounds.cursor_max,
        });
    }
    if u64::from(words.extents) > bounds.total_extents {
        return Err(ReleaseWordRefusal::ExtentsAboveVolume {
            extents: words.extents,
            total: bounds.total_extents,
        });
    }
    if words.root != bounds.root_recorded
        && !root_extent_of(words.root.0, bounds).is_some_and(|e| grant.contains(e))
    {
        return Err(ReleaseWordRefusal::RootOutsideGrant { addr: words.root.0 });
    }
    Ok(())
}

/// Why a RAM grant refuses to drop an extent for a return
/// ([`RegionGrant::drop_returned`]): it holds a live image, or its free is
/// still parked on the region's tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantHeld {
    Claimed,
    Pending,
}

impl GrantHeld {
    /// The refusal's word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed (a live image)",
            Self::Pending => "pending (its free is parked on the region's tail — not returnable until the tail passes it)",
        }
    }
}

/// One region's extent grant in RAM (§5.3.3): the manager carved it, the
/// page names its UNCLAIMED remainder, tree 0's `extent_grant` record
/// names the whole of it. Claims take the lowest unclaimed extent so the
/// remainder stays as few runs as the grants that produced it; a free is
/// parked on THIS region's tail and, once released, returned to the
/// manager in a batch at the checkpoint cadence.
#[derive(Debug, Default)]
pub struct RegionGrant {
    unclaimed: std::collections::BTreeSet<u64>,
    claimed: std::collections::BTreeSet<u64>,
    /// `(extent, gate seq)` — frees parked on this region's tail.
    pending: Vec<(u64, u64)>,
    /// Released past the tail, awaiting `ReturnExtents`.
    returnable: Vec<u64>,
    /// The in-window `alloc` records the replay folded (`claim_exact`) —
    /// the own-residue pool census's shield, taken once at the open.
    window_claims: std::collections::BTreeSet<u64>,
    /// Extents ever granted to this region (`extent_grant_extents`).
    pub granted: u64,
    /// Extents returned to the manager (`returned` in the closure law).
    pub returned: u64,
    /// The unclaimed count at the last refill decision — the 50 % law's
    /// reference (`granted_since_refill`).
    pub refill_reference: u64,
    /// Extents the §4.7 heap admission PROMISED against this grant for
    /// the SMOs a leased slot's leaves will need (review round 1, Issue
    /// 10): a leased leaf's SMO draws the GRANT, never the bitmap, so its
    /// promise reserves grant headroom — `headroom() = unclaimed −
    /// promised`. The ledger is SHARED with the region's leaves' dirty
    /// halves ([`Self::promise_ledger`] → `NodeDirty::set_promise_ledger`)
    /// so a leased leaf's promise lifecycle is the heap's verbatim —
    /// promised at admission, released when its SMO takes the overlay or
    /// the promise is retracted — on this counter instead of the heap's.
    promised: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl RegionGrant {
    /// Adopt the runs a grant answered. An extent this grant already
    /// HOLDS — claimed (a live image), parked on the tail, or in the
    /// return batch — is never re-entered into the unclaimed set (PR 13,
    /// defect 5's grant half): the manager's §5.3.5 verbatim answer and
    /// its coalesced carve both describe the caller's remainder as it
    /// stood at the caller's last page WRITE, and a claim that landed
    /// since is inside those runs; re-unclaiming it handed a live node's
    /// extent to the next mint (two appenders' frames under one node_seq
    /// at one extent) or trimmed it into a `ReturnExtents` while its node
    /// stood. Only a genuinely new extent counts as granted.
    pub fn add_runs(&mut self, runs: &[GrantRun]) {
        for r in runs {
            for e in r.start..r.start + u64::from(r.len) {
                if self.claimed.contains(&e)
                    || self.pending.iter().any(|(p, _)| *p == e)
                    || self.returnable.contains(&e)
                {
                    continue;
                }
                if self.unclaimed.insert(e) {
                    self.granted += 1;
                }
            }
        }
        self.refill_reference = self.unclaimed.len() as u64;
    }

    /// Recovery: the whole grant (tree 0's record) against the page's
    /// unclaimed remainder — everything else is claimed. **The page word
    /// is adopted ∩ the record** (PR 13g review round 2, Issue 16a): the
    /// record is the manager's truth (`record ⊆ claimed ∪ unclaimed ∪
    /// pending ∪ returnable`, PR 3's law), and a page-named extent the
    /// record no longer grants was RETURNED after that page write — the
    /// manager may hold it in another appender's grant by now; adopting
    /// it would make this region a second custodian. Answers the grant
    /// and the count of page-named extents dropped
    /// (`appender_stale_page_words_dropped`).
    pub fn recover(
        record_extents: impl IntoIterator<Item = u64>,
        unclaimed: &[GrantRun],
    ) -> (Self, u64) {
        let record: std::collections::BTreeSet<u64> = record_extents.into_iter().collect();
        let mut g = Self::default();
        let mut dropped = 0u64;
        for r in unclaimed {
            for e in r.start..r.start + u64::from(r.len) {
                if record.contains(&e) {
                    g.unclaimed.insert(e);
                } else {
                    dropped += 1;
                }
            }
        }
        for e in record {
            g.granted += 1;
            if !g.unclaimed.contains(&e) {
                g.claimed.insert(e);
            }
        }
        g.refill_reference = g.unclaimed.len() as u64;
        (g, dropped)
    }

    /// Claim the lowest extent of the SMALLEST unclaimed run (ties: the
    /// lowest run) — PR 13g, F-R5: a recycled image (a single wherever
    /// its predecessor stood) is consumed by the next SMO before it can
    /// fragment the page's word, and the grant's contiguous runs stay
    /// whole for as long as the pool holds a fragment. The lowest-first
    /// claim of PR 3 kept the remainder "as few runs as the grants that
    /// produced it" only while nothing ever re-entered the pool.
    pub fn claim(&mut self) -> Option<u64> {
        let runs = self.unclaimed_runs();
        let e = runs
            .iter()
            .min_by_key(|r| (r.len, r.start))
            .map(|r| r.start)?;
        self.unclaimed.remove(&e);
        self.claimed.insert(e);
        Some(e)
    }

    /// Recovery fold of an in-window `alloc(extent)` record: the extent is
    /// claimed whatever the page said (the page predates the claim) — and
    /// whatever an EARLIER `free(extent)` of the same window said: an
    /// extent this region retired, recycled and claimed again inside one
    /// window (PR 13g) folds to CLAIMED, never to pending AND claimed at
    /// once (a park the next tail would have released while the image
    /// stood). The claim is remembered as the WINDOW's
    /// ([`Self::take_window_claims`]) — the own-residue pool census's
    /// shield.
    pub fn claim_exact(&mut self, extent: u64) {
        self.unclaimed.remove(&extent);
        self.pending.retain(|(e, _)| *e != extent);
        self.returnable.retain(|e| *e != extent);
        self.claimed.insert(extent);
        self.window_claims.insert(extent);
    }

    /// The in-window claims the replay folded ([`Self::claim_exact`]),
    /// taken once by the own-residue pool census.
    pub fn take_window_claims(&mut self) -> std::collections::BTreeSet<u64> {
        std::mem::take(&mut self.window_claims)
    }

    /// **The own-residue POOL census's restore** (PR 13g review round 1,
    /// Issue 2): a CLAIMED extent no tree of this mount reaches and no
    /// in-window claim named is the POOL the page could not name (the
    /// page names the remainder's largest `GRANT_RUNS_MAX` runs; the rest
    /// stayed unclaimed in RAM and the crash-rejoin's `recover` landed
    /// them claimed) — back to UNCLAIMED. Answers whether it moved.
    pub fn unclaim(&mut self, extent: u64) -> bool {
        if self.claimed.remove(&extent) {
            self.unclaimed.insert(extent);
            true
        } else {
            false
        }
    }

    /// A claim whose build was abandoned before publication.
    pub fn release_unpublished(&mut self, extent: u64) {
        if self.claimed.remove(&extent) {
            self.unclaimed.insert(extent);
        }
    }

    /// Park a freed image gated on `gate_seq` (a position in this
    /// region's ring).
    pub fn free_pending(&mut self, extent: u64, gate_seq: u64) {
        if self.claimed.remove(&extent) {
            self.pending.push((extent, gate_seq));
        }
    }

    /// Recovery fold of an in-window `free(extent)` record.
    pub fn free_exact(&mut self, extent: u64, gate_seq: u64) {
        self.claimed.remove(&extent);
        self.unclaimed.remove(&extent);
        if !self.pending.iter().any(|(e, _)| *e == extent) {
            self.pending.push((extent, gate_seq));
        }
    }

    /// The root-swap carve-out at the region's replay
    /// ([`super::alloc_ext::ExtentAllocator::park_replayed_frees`]'s law
    /// for a grant): a replayed `free` whose extent is one of the mounted
    /// `live_roots` is the unpublished swap's retirement of a root the
    /// mount replays THROUGH — it goes back to CLAIMED (the image is
    /// live), never to the returnable batch. Returns how many.
    pub fn unpark_live_roots(&mut self, live_roots: &std::collections::BTreeSet<u64>) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < self.pending.len() {
            if live_roots.contains(&self.pending[i].0) {
                let (e, _) = self.pending.swap_remove(i);
                self.claimed.insert(e);
                n += 1;
            } else {
                i += 1;
            }
        }
        n
    }

    /// A slot HANDOVER's custody transfer OUT (design-symmetric-metadata
    /// §5.1.4 / §5.8.5 C13): the departing lessee's grant stops claiming
    /// the slot tree's live images — they are the manager's (untracked,
    /// like its own images) until the requester's grant claims them. The
    /// bitmap bits stay set (the images are live); the closure law reads
    /// them as RETURNED. Answers the extents that were claimed here.
    pub fn transfer_out(&mut self, extents: &[u64]) -> Vec<u64> {
        let mut out = Vec::new();
        for e in extents {
            if self.claimed.remove(e) {
                self.returned += 1;
                out.push(*e);
            }
        }
        out
    }

    /// The transfer IN: the requester's grant claims the slot tree's live
    /// images (granted by transfer — `granted` counts them).
    pub fn transfer_in(&mut self, extents: &[u64]) {
        for e in extents {
            self.unclaimed.remove(e);
            if self.claimed.insert(*e) {
                self.granted += 1;
            }
        }
    }

    /// The claimed set (fsck C13's candidate population).
    pub fn claimed_extents(&self) -> Vec<u64> {
        self.claimed.iter().copied().collect()
    }

    /// Whether `extent` is CLAIMED (holds an image, by this grant's
    /// account).
    pub fn is_claimed(&self, extent: u64) -> bool {
        self.claimed.contains(&extent)
    }

    /// The coverage gate: a durable tail of `tail` releases every parked
    /// free whose gate it covers into the returnable batch.
    pub fn advance_durable(&mut self, tail: u64) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].1 <= tail {
                let (e, _) = self.pending.swap_remove(i);
                self.returnable.push(e);
                n += 1;
            } else {
                i += 1;
            }
        }
        n
    }

    /// Drain the UNCLAIMED remainder — the region release's return
    /// (§5.1.3): the extents go back to the manager, never to an image.
    pub fn take_unclaimed(&mut self) -> Vec<u64> {
        let v: Vec<u64> = std::mem::take(&mut self.unclaimed).into_iter().collect();
        self.returned += v.len() as u64;
        self.refill_reference = 0;
        v
    }

    /// Drain the returnable batch (the `ReturnExtents` payload).
    pub fn take_returnable(&mut self) -> Vec<u64> {
        let mut v = std::mem::take(&mut self.returnable);
        v.sort_unstable();
        self.returned += v.len() as u64;
        v
    }

    /// Put a batch back after a failed return.
    pub fn restore_returnable(&mut self, extents: Vec<u64>) {
        self.returned = self.returned.saturating_sub(extents.len() as u64);
        self.returnable.extend(extents);
    }

    /// Put extents taken by [`Self::take_returnable`] / [`Self::
    /// take_unclaimed`] back as CLAIMED (PR 13's live-image belt): the
    /// caller found a live node of this mount at each — whatever ledger
    /// step called it free was wrong, and the image stays in this grant's
    /// custody rather than leaving for another appender to overwrite.
    pub fn reclaim_as_claimed(&mut self, extents: &[u64]) {
        for e in extents {
            self.returned = self.returned.saturating_sub(1);
            self.returnable.retain(|r| r != e);
            self.unclaimed.remove(e);
            self.claimed.insert(*e);
        }
    }

    /// The unclaimed remainder as ascending runs — the WHOLE remainder
    /// (the RAM pool); the page names [`Self::page_runs`] of it.
    pub fn unclaimed_runs(&self) -> Vec<GrantRun> {
        let mut runs: Vec<GrantRun> = Vec::new();
        for &e in &self.unclaimed {
            match runs.last_mut() {
                Some(r) if r.start + u64::from(r.len) == e => r.len += 1,
                _ => runs.push(GrantRun { start: e, len: 1 }),
            }
        }
        runs
    }

    /// The runs the PAGE names: the whole remainder when it fits the
    /// page's [`GRANT_RUNS_MAX`], else its largest runs
    /// ([`page_runs_of`]). The rest stay UNCLAIMED in RAM, unnamed — the
    /// transient pool a storm's recycled images form between one cadence
    /// and the next flush pass (each a single wherever its predecessor
    /// stood; the smallest-run claim consumes them first). On a
    /// crash-class open an unnamed extent recovers as CLAIMED (tree 0's
    /// record minus the page's word): the recoverer's orphan census
    /// returns it on the death path (§5.9 step 8), fsck C13 on a rejoin
    /// — a round trip on a CRASH, bounded by one cycle's retirements,
    /// where PR 3's trim (review round 1, Issue 9: the excess moved to the
    /// returnable batch so the page named every unclaimed extent) made
    /// the same round trip on EVERY cadence of a live storm — F-R5's
    /// churn (PR 13g).
    pub fn page_runs(&self) -> Vec<GrantRun> {
        page_runs_of(&self.unclaimed_runs())
    }

    /// **The recycle** (PR 13g, F-R5 — §5.3.3 as built): `released` is a
    /// batch [`Self::take_returnable`] drained — the images this region
    /// retired and its tail released, the live-image belt already run
    /// over it — and the batch re-enters this region's OWN unclaimed pool,
    /// lowest first, while the pool is below `keep` (the derived grant
    /// size; `u64::MAX` on a pressure-driven cycle); what does not fit is
    /// answered as the SURPLUS the cadence ships as `ReturnExtents`. A
    /// compaction's claim + retire is then a net-zero move inside the
    /// grant — before it every retired image travelled to the manager and
    /// came back one cycle later as a fresh carve (a ring-0 control entry
    /// + barrier each way). A recycled extent never left the grant: the
    /// `returned` the drain counted for it is taken back; `granted` never
    /// moves.
    pub fn recycle(&mut self, released: Vec<u64>, keep: u64) -> Vec<u64> {
        let mut released = released;
        released.sort_unstable();
        let mut surplus = Vec::with_capacity(released.len());
        for e in released {
            if self.unclaimed() < keep && !self.claimed.contains(&e) && self.unclaimed.insert(e) {
                self.returned = self.returned.saturating_sub(1);
            } else {
                surplus.push(e);
            }
        }
        surplus
    }

    /// **The pool's SHRINK** (PR 13g review round 1, Issue 5): take the
    /// unclaimed extents above `target` out of the pool — the SMALLEST
    /// runs first (the `claim()` order: a fragment leaves before a run is
    /// broken), the lowest extents of the run that straddles the mark —
    /// and answer them for the cadence's `ReturnExtents`. Nothing below
    /// `target` moves; `target` is never below the promised headroom (the
    /// caller's law). The surplus reads RETURNED from here (the closure
    /// law `granted ≡ held + returned + unclaimed`, [`Self::
    /// take_returnable`]'s discipline — a failed return's
    /// [`Self::restore_returnable`] takes the count back).
    pub fn shrink_to(&mut self, target: u64) -> Vec<u64> {
        let mut surplus = self.unclaimed().saturating_sub(target);
        let mut out = Vec::with_capacity(surplus as usize);
        if surplus == 0 {
            return out;
        }
        let mut runs = self.unclaimed_runs();
        runs.sort_by(|a, b| a.len.cmp(&b.len).then(a.start.cmp(&b.start)));
        for r in runs {
            if surplus == 0 {
                break;
            }
            let take = u64::from(r.len).min(surplus);
            for e in r.start..r.start + take {
                if self.unclaimed.remove(&e) {
                    out.push(e);
                }
            }
            surplus -= take;
        }
        out.sort_unstable();
        self.returned += out.len() as u64;
        out
    }

    /// Undo a [`Self::shrink_to`] whose page rewrite failed (review round
    /// 2, Issue 16b): the surplus goes back into the pool — which the old
    /// page word, still on the device, names truly — and the count the
    /// shrink moved to `returned` comes back.
    pub fn restore_unclaimed(&mut self, extents: Vec<u64>) {
        for e in extents {
            if self.unclaimed.insert(e) {
                self.returned = self.returned.saturating_sub(1);
            }
        }
    }

    /// Grant headroom the §4.7 admission may promise against: the
    /// unclaimed remainder less what earlier admissions already promised
    /// (review round 1, Issue 10 — a leased leaf's SMO draws THIS grant,
    /// never the heap ledger).
    pub fn headroom(&self) -> u64 {
        self.unclaimed().saturating_sub(self.promised())
    }

    /// Promise `n` extents of the headroom to admitted-but-unflushed
    /// SMOs of this region's leaves (the leaves' dirty halves do this
    /// through the shared ledger in product code).
    pub fn promise(&self, n: u64) {
        self.promised
            .fetch_add(n, std::sync::atomic::Ordering::AcqRel);
    }

    /// Extents currently promised (`grant_promised`; 0 at quiesce).
    pub fn promised(&self) -> u64 {
        self.promised.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Retract `n` promises (a refused member, a node that flushed
    /// without the SMO its admission projected).
    pub fn retract_promise(&self, n: u64) {
        let _ = self.promised.fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |cur| Some(cur.saturating_sub(n)),
        );
    }

    /// The promise ledger a leased leaf's dirty half is pointed at
    /// (`NodeDirty::set_promise_ledger`): its `promise` / `retract` /
    /// `release` land here instead of on the heap's `heap_promised`.
    pub fn promise_ledger(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        std::sync::Arc::clone(&self.promised)
    }

    /// Drop `extent` from the sets a return may take it from — UNCLAIMED
    /// and RETURNABLE: a `ReturnExtents` of an extent an in-process
    /// region's RAM grant still holds (review round 1, Issue 11) — the
    /// bitmap and the RAM grant must not disagree, or a later `claim()`
    /// hands out an extent the manager re-granted. `Err(Held)` when the
    /// extent is CLAIMED (it holds an image) or PENDING (its free is parked
    /// on this region's tail — until the tail passes the free record a
    /// crash replays through the structure that still routes to the
    /// retired image, the §4.7 coverage gate; review round 2, Issue 17 —
    /// it becomes returnable only through `advance_durable`); `Ok(true)`
    /// when it was dropped, `Ok(false)` when no set held it.
    pub fn drop_returned(&mut self, extent: u64) -> std::result::Result<bool, GrantHeld> {
        if self.claimed.contains(&extent) {
            return Err(GrantHeld::Claimed);
        }
        if self.pending.iter().any(|(e, _)| *e == extent) {
            return Err(GrantHeld::Pending);
        }
        let mut dropped = self.unclaimed.remove(&extent);
        let before = self.returnable.len();
        self.returnable.retain(|e| *e != extent);
        dropped |= self.returnable.len() != before;
        if dropped {
            self.returned += 1;
        }
        Ok(dropped)
    }

    pub fn unclaimed(&self) -> u64 {
        self.unclaimed.len() as u64
    }

    pub fn claimed(&self) -> u64 {
        self.claimed.len() as u64
    }

    pub fn pending(&self) -> u64 {
        self.pending.len() as u64
    }

    pub fn returnable(&self) -> u64 {
        self.returnable.len() as u64
    }

    /// The HELD term of the closure law: every granted extent the region
    /// still holds — live images, frees parked on its tail, releases not
    /// yet returned.
    pub fn held(&self) -> u64 {
        self.claimed() + self.pending() + self.returnable()
    }

    /// Whether `extent` is this grant's — claimed, unclaimed, parked or
    /// returnable (`KvMetaBackend::grant_holds`, the durable-vs-RAM law's
    /// probe).
    pub fn contains(&self, extent: u64) -> bool {
        self.claimed.contains(&extent)
            || self.unclaimed.contains(&extent)
            || self.pending.iter().any(|(e, _)| *e == extent)
            || self.returnable.contains(&extent)
    }

    /// The proactive-refill law (§5.3.3, the `alloc_lane` ahead-refill
    /// shape): a refill is due once HALF of what the last grant left has
    /// been consumed.
    pub fn refill_due(&self) -> bool {
        self.unclaimed() * 2 <= self.refill_reference
    }
}

// ---- Device I/O (every read and write through `crate::uring_fs`) ------------

/// Read one page-sized image at `offset` (zero-extended on a short read —
/// zeros classify as [`PageRead::Blank`]).
pub async fn read_page(path: &Path, offset: u64) -> Result<Vec<u8>, KvError> {
    let got = crate::uring_fs::read_at(path, offset, APPENDER_PAGE_LEN).await?;
    let mut img = vec![0u8; APPENDER_PAGE_LEN];
    let n = got.len().min(APPENDER_PAGE_LEN);
    img[..n].copy_from_slice(&got[..n]);
    Ok(img)
}

/// Write one page image at `offset`.
pub async fn write_page(path: &Path, offset: u64, img: Vec<u8>) -> Result<(), KvError> {
    crate::uring_fs::write_at(path, offset, bytes::Bytes::from(img)).await?;
    Ok(())
}

/// ZERO the extents a ring is carved from, BEFORE any page names them
/// (PR 3 review round 2; the ring carve's law): the heap hands a fresh
/// ring — a rejoin's, a wire join's, a growth segment — the extents a
/// predecessor incarnation's ring occupied, lowest-free-first, with that
/// ring's entries still on the device, checksummed, at lap 0 like the new
/// ring's own. The §4.1 replay chain of the new incarnation then walks
/// past its own head into them: a predecessor's `+ref` at a higher
/// position out-votes this mount's acked release (per-key LWW by ring
/// position — an acked delete undone), a predecessor's `free(extent)`
/// folds into the grant and is RETURNED (a live image's bit cleared). A
/// zeroed page verifies nothing and replays to nothing, so a carved ring
/// replays exactly what its own incarnation wrote. One sequential write
/// per extent in `ZERO_CHUNK` pieces (the ring floor is 512 KiB; the
/// ceiling the solo ring's) — a control-plane act at the carve, never on
/// a hot path.
pub async fn zero_extents(
    path: &Path,
    extents: &[super::superblock::ExtentRef],
) -> Result<(), KvError> {
    const ZERO_CHUNK: u64 = 4 * 1024 * 1024;
    let zeros = bytes::Bytes::from(vec![0u8; ZERO_CHUNK as usize]);
    for ext in extents {
        let mut off = ext.start;
        while off < ext.end() {
            let n = (ext.end() - off).min(ZERO_CHUNK);
            crate::uring_fs::write_at(path, off, zeros.slice(..n as usize)).await?;
            off += n;
        }
    }
    Ok(())
}

/// The newest valid page over an appender's four slots at `offsets`
/// (the ring-side pair may be absent — a page pair whose ring is not yet
/// known reads its two directory slots only).
pub async fn read_newest_page(
    path: &Path,
    offsets: &[u64],
) -> Result<Option<(usize, AppenderPage)>, KvError> {
    let mut images = Vec::with_capacity(offsets.len());
    for off in offsets {
        images.push(read_page(path, *off).await?);
    }
    newest_valid(&images)
}

/// One appender's directory presence: its id, where its page slots live
/// and the newest valid page (if any).
#[derive(Debug, Clone)]
pub struct AppenderEntry {
    pub appender_id: u32,
    /// `[A, B]` device offsets.
    pub dir_offsets: [u64; 2],
    /// The newest valid page over `[A, B, R0, R1]` (R0/R1 read only when
    /// a valid directory page names a first ring segment).
    pub page: Option<AppenderPage>,
    /// The valid images of the DIRECTORY pair `[A, B]` themselves — what
    /// [`dir_named_mask`] reads: which of the two names the ring the
    /// mount is about to use.
    pub dir_pages: [Option<AppenderPage>; 2],
}

/// One directory image's valid page. Called only after [`newest_valid`]
/// judged the same images, so a `ForeignLayout` was already refused and
/// never reaches the `None` arm here.
fn valid_page(img: &[u8]) -> Option<AppenderPage> {
    match classify_page(img) {
        PageRead::Valid(p) => Some(p),
        PageRead::Blank | PageRead::Corrupt(_) | PageRead::ForeignLayout { .. } => None,
    }
}

/// Directory-pair confirmation mask: bit 0 = slot A's valid image names
/// the region's CURRENT ring table, bit 1 = slot B's. Both set =
/// [`DIR_NAMED_ALL`].
pub const DIR_NAMED_ALL: u8 = 0b11;

/// The mask over the directory pair's valid images for a region whose
/// ring table is `table`.
pub fn dir_named_mask(dir_pages: &[Option<AppenderPage>; 2], table: &[ExtentRef]) -> u8 {
    let mut mask = 0u8;
    for (i, p) in dir_pages.iter().enumerate() {
        if p.as_ref().is_some_and(|p| p.segments == table) {
            mask |= 1 << i;
        }
    }
    mask
}

/// **The DIRECTORY-FIRST page-slot law** (review round 1, Issue 1 — the
/// invariant `read_directory` rests on: "a directory image always names
/// the region's current first segment"). The slots ONE logical page write
/// lands in, given the pair's confirmation `mask` and the generation the
/// first image will carry: while a directory slot does not name the
/// current table — after a join onto a fresh ring, a growth, a leave —
/// every such slot is written (A then B, one generation apart), so the
/// pair alone finds the ring and a torn one falls back to the other
/// naming the SAME table (a table-changing page whose only copy tore
/// would fall back to a predecessor naming a ring that cannot decode the
/// positions written since); once both name it, the four-slot rotation
/// `page_slot_for(generation)` — the ring-side slots are reachable
/// through either directory image, so the design's two predecessors of
/// slack hold for appenders ≥ 1 as for appender 0.
pub fn page_write_slots(mask: u8, generation: u64) -> Vec<usize> {
    if mask & DIR_NAMED_ALL == DIR_NAMED_ALL {
        vec![page_slot_for(generation)]
    } else {
        (0..2).filter(|i| mask & (1 << i) == 0).collect()
    }
}

/// Whole-directory walks this process performed (`appender_directory_
/// reads`; PR 13g review round 1, Issue 13): every manager verb on a wire
/// appender pays its page read here — a verb's count of them is a term
/// of the manager's verb wall (F-B1), so a wire `ExtentGrant` performs
/// exactly ONE (pinned).
static APPENDER_DIRECTORY_READS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The process-wide census of [`read_directory`] walks.
pub fn directory_reads() -> u64 {
    APPENDER_DIRECTORY_READS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Walk the appender directory of a bit-17 volume: appender 0 from the
/// fixed journal extent, appenders ≥ 1 from the chain
/// `superblock.appender_dir` names. Every pair slot of every chain extent
/// is read (a Free/blank pair is an unallocated id). Refuses a chain whose
/// header page does not verify — the directory is a checksummed unit.
pub async fn read_directory(
    path: &Path,
    sb: &super::superblock::SuperblockV3,
) -> Result<Vec<AppenderEntry>, KvError> {
    APPENDER_DIRECTORY_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut out = Vec::new();
    // Appender 0: all four slots sit in the fixed extent.
    let offs0 = appender0_page_offsets(&sb.journal);
    let mut images0 = Vec::with_capacity(offs0.len());
    for off in &offs0 {
        images0.push(read_page(path, *off).await?);
    }
    let page0 = newest_valid(&images0)?.map(|(_, p)| p);
    out.push(AppenderEntry {
        appender_id: 0,
        dir_offsets: [offs0[0], offs0[1]],
        page: page0,
        dir_pages: [valid_page(&images0[0]), valid_page(&images0[1])],
    });
    if sb.appender_dir.len == 0 {
        return Ok(out);
    }
    let mut extent = sb.appender_dir;
    let mut chain_index = 0u32;
    let mut next_id = 1u32;
    // Bounded by the heap: a chain longer than the heap's extents is a
    // cycle.
    let max_chain = sb.total_extents().max(1);
    while extent.len != 0 {
        if u64::from(chain_index) >= max_chain {
            return Err(KvError::Corrupt(format!(
                "{}: appender directory chain exceeds the heap's {max_chain} extents (a cycle)",
                path.display()
            )));
        }
        let hdr = DirHeader::decode(&read_page(path, dir_header_offset(&extent)).await?)?;
        if hdr.chain_index != chain_index {
            return Err(KvError::Corrupt(format!(
                "{}: appender directory extent at {:#x} carries chain index {} (expected \
                 {chain_index})",
                path.display(),
                extent.start,
                hdr.chain_index
            )));
        }
        let pairs = u64::from(hdr.pairs).min(dir_pairs_per_extent(extent.len));
        for pair in 0..pairs {
            let offs = dir_pair_offsets(&extent, pair);
            let mut images = vec![
                read_page(path, offs[0]).await?,
                read_page(path, offs[1]).await?,
            ];
            // The ring-side pair, once a directory image names the ring.
            if let Some((_, p)) = newest_valid(&images)? {
                if let Some(first) = p.segments.first() {
                    for off in ring_side_offsets(first) {
                        images.push(read_page(path, off).await?);
                    }
                }
            }
            let page = newest_valid(&images)?.map(|(_, p)| p);
            out.push(AppenderEntry {
                appender_id: next_id,
                dir_offsets: offs,
                page,
                dir_pages: [valid_page(&images[0]), valid_page(&images[1])],
            });
            next_id += 1;
        }
        extent = hdr.next;
        chain_index += 1;
    }
    Ok(out)
}

/// The appender directory's extent CHAIN with each extent's header, in
/// chain order (empty on a volume with no directory) — what the
/// manager's `JoinAppender` grows.
pub async fn read_directory_chain(
    path: &Path,
    sb: &super::superblock::SuperblockV3,
) -> Result<Vec<(ExtentRef, DirHeader)>, KvError> {
    let mut out = Vec::new();
    if sb.appender_dir.len == 0 {
        return Ok(out);
    }
    let mut extent = sb.appender_dir;
    let max_chain = sb.total_extents().max(1);
    while extent.len != 0 {
        if out.len() as u64 >= max_chain {
            return Err(KvError::Corrupt(format!(
                "{}: appender directory chain exceeds the heap's {max_chain} extents (a cycle)",
                path.display()
            )));
        }
        let hdr = DirHeader::decode(&read_page(path, dir_header_offset(&extent)).await?)?;
        let next = hdr.next;
        out.push((extent, hdr));
        extent = next;
    }
    Ok(out)
}

/// The four page-slot offsets of `entry`'s appender given its ring's
/// first segment: `[A, B, R0, R1]` (appender 0's R0/R1 are its fixed
/// extent's pages 2 and 3; an appender ≥ 1's are its first segment's).
pub fn page_slot_offsets(
    sb: &super::superblock::SuperblockV3,
    appender_id: u32,
    dir_offsets: [u64; 2],
    first_segment: &ExtentRef,
) -> [u64; APPENDER_PAGE_SLOTS] {
    if appender_id == 0 {
        appender0_page_offsets(&sb.journal)
    } else {
        let rs = ring_side_offsets(first_segment);
        [dir_offsets[0], dir_offsets[1], rs[0], rs[1]]
    }
}

// ---- The mounted regions ----------------------------------------------------

/// The per-appender FLUSH CEILING, ms (KD-SYM-10, §5.7.3): every dirty
/// leaf of every slot tree an appender leases is flushed — bset appended
/// and barriered — within this many ms of the record that dirtied it. It
/// is the checkpoint LANDING ceiling of the cadence in force
/// (`checkpoint_landing_ceiling_ms`: `CHECKPOINT_MAX_AGE_MS` + two tick
/// periods — 1,100 ms at the shipped 50 ms flush), the ONE derivation the
/// reader's qualify term already rests on: KD-SYM-10's "within
/// `CHECKPOINT_MAX_AGE_MS`" names the cadence TRIGGER, which the tick
/// fires AT (`elapsed ≥ ceiling`), so a leaf dirtied ε after a checkpoint
/// is `trigger + pass − ε` old at its covering barrier and the trigger
/// alone reads a healthy mount as overrunning (review round 2, Issue 22:
/// 3 overruns in 3.5 s of a steady stream, ages 1,013–1,060 ms). The
/// two tick terms are the tick wait and the bounded maintenance drain
/// that precedes the decision in the same tick; the pass's own device
/// time is what the audit MEASURES against them.
pub fn appender_flush_ceiling_ms(flush_interval_ms: u64) -> u64 {
    super::checkpoint::checkpoint_landing_ceiling_ms(flush_interval_ms)
}

/// The CAP on the flush-ceiling audit's SERVICE exclusion, ms (PR 13c,
/// F-B1 — review round 1, Issue 1c): a leaf that aged under a service
/// hold of the SMO mutex (a wire appender's slot grant / release, a
/// transfer's adoption, a projection refresh, a region's release) is
/// excused the hold's overlap with its dirty window, but never more than
/// ONE landing ceiling — the very contract the audit excuses against.
/// The ceiling is a cross-plane promise (the free-grace qualify term and
/// the `=0` reader's staleness input read `checkpoint_landing_ceiling_ms`
/// as the writer's landing bound); a hold longer than the ceiling itself
/// is the stall class those consumers must SEE, so the excess counts as
/// the overrun. The recovery class keeps its own published cap
/// (`appender_recovery_bound_ms`). Published live as
/// `appender_flush_ceiling_service_cap_ms`.
pub fn appender_flush_ceiling_service_cap_ms(flush_interval_ms: u64) -> u64 {
    appender_flush_ceiling_ms(flush_interval_ms)
}

/// `SQUEEZEFS_TEST_SYM_APPENDER_SLOTS` — the PR-2 declared static
/// partition (a harness seam standing in for PR 4's lease gate).
pub const TEST_APPENDER_SLOTS_ENV: &str = "SQUEEZEFS_TEST_SYM_APPENDER_SLOTS";

/// Parse the declared partition: `<id>:<forest slot>[,<slot>…][;<id>:…]`,
/// ids ≥ 1, slots inside the forest namespace and named once. Empty /
/// unset = no content appender besides the manager.
pub fn declared_partition() -> Result<
    std::collections::BTreeMap<u32, std::collections::BTreeSet<super::record::ForestSlot>>,
    KvError,
> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(raw) = std::env::var(TEST_APPENDER_SLOTS_ENV) else {
        return Ok(out);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(out);
    }
    let mut seen: std::collections::BTreeSet<super::record::ForestSlot> =
        std::collections::BTreeSet::new();
    for part in raw.split(';') {
        let (id, slots) = part.split_once(':').ok_or_else(|| {
            KvError::Corrupt(format!(
                "{TEST_APPENDER_SLOTS_ENV}: expected `<id>:<slot>[,<slot>…]`, got {part:?}"
            ))
        })?;
        let id: u32 = id.trim().parse().map_err(|e| {
            KvError::Corrupt(format!(
                "{TEST_APPENDER_SLOTS_ENV}: bad appender id {id:?}: {e}"
            ))
        })?;
        if id == 0 {
            return Err(KvError::Corrupt(format!(
                "{TEST_APPENDER_SLOTS_ENV}: appender 0 is the manager and leases the complement"
            )));
        }
        let set: &mut std::collections::BTreeSet<super::record::ForestSlot> =
            out.entry(id).or_default();
        for s in slots.split(',') {
            let slot: super::record::ForestSlot = s.trim().parse().map_err(|e| {
                KvError::Corrupt(format!("{TEST_APPENDER_SLOTS_ENV}: bad slot {s:?}: {e}"))
            })?;
            if slot == super::record::NATIVE_FOREST_SLOT || slot > super::record::FOREST_SLOT_MAX {
                return Err(KvError::Corrupt(format!(
                    "{TEST_APPENDER_SLOTS_ENV}: slot {slot} is the native slot or outside the \
                     forest namespace"
                )));
            }
            if !seen.insert(slot) {
                return Err(KvError::Corrupt(format!(
                    "{TEST_APPENDER_SLOTS_ENV}: slot {slot} is named twice"
                )));
            }
            set.insert(slot);
        }
    }
    Ok(out)
}

/// One appender region this mount holds: its page slots, the RAM copy of
/// its page, its ring (swapped on growth — never in place) and the
/// per-region ledger the checkpoint cycle maintains.
///
/// **Lock order (PR 13i):** a site that holds both takes `grant` BEFORE
/// `page` — the grant writers name the remainder on the page under the
/// grant guard (`manager_extent_grant_class`, the joiner's remainder
/// naming), the checkpoint task's page writer (`write_appender_pages`)
/// reads the remainder under the grant guard before it locks the page,
/// and the `.stats` reader follows them. The reverse order deadlocked the
/// operator's stats poll against the manager's checkpoint task (F-C4,
/// found by the growth contract under the park-kick cycle; pinned by
/// `sym_appender_tests::the_stats_reader_never_deadlocks_against_a_grants_page_update`)
/// and, at the page writer, a joined writer's cadence against its own
/// stats poll (fix round 1, Issue 1; `sym_n_daemon_tests::a_joiners_stats_
/// reader_never_deadlocks_against_its_own_checkpoint_page_writer`). The
/// static rail `tests/appender_lock_order_tests.rs` refuses any `page`
/// guard whose block acquires `grant`. Neither guard is ever held across
/// an await.
pub struct AppenderRegion {
    pub id: u32,
    /// `[A, B, R0, R1]` device offsets.
    pub page_offsets: [u64; APPENDER_PAGE_SLOTS],
    /// The page as last written (generation, term, roots, tail).
    pub page: std::sync::Mutex<AppenderPage>,
    /// The ring. Region 0's IS the backend's fixed ring; a region ≥ 1's
    /// is replaced by [`super::journal::JournalRing::grown_with`] on a
    /// drained stall.
    pub ring: arc_swap::ArcSwap<super::journal::JournalRing>,
    /// Forest slots this region leases: the declared partition on an
    /// unarmed forest (empty for the manager, which leases the
    /// complement); under an ARMED symmetric plane (PR 4) the live lease
    /// set — swapped whole at every acquire / release, read latch-free
    /// by the conveyor's region routing.
    pub leases: arc_swap::ArcSwap<std::collections::BTreeSet<super::record::ForestSlot>>,
    /// The one-writer mutex of `leases` (review round 2, Issue 5): every
    /// RMW of the set — `add_lease` / `drop_lease` — runs under it, held
    /// for the RMW only, never across an await.
    pub leases_writer: std::sync::Mutex<()>,
    /// The tail the last written page named.
    pub last_tail: std::sync::atomic::AtomicU64,
    /// The tail the last COMPLETED barrier made durable — what this
    /// ring's `reusable_upto` follows.
    pub durable_tail: std::sync::atomic::AtomicU64,
    /// Ring-admission parks on this region's ring (`journal_full_stalls`
    /// attributed per appender).
    pub stalls: std::sync::atomic::AtomicU64,
    /// `stalls` as of the last growth decision — growth fires when it moved.
    pub stalls_at_last_grow: std::sync::atomic::AtomicU64,
    /// The ring size at which the manager last DECLINED this region's
    /// `GrowRing` (`u64::MAX` = none): a joiner asks again only once its
    /// ring changed or a fresh stall arrived — never a verb per cycle at
    /// a ceiling or an exhausted budget the manager already named
    /// (PR 13i: the sector-pad law's ring demand reaches both inside one
    /// storm on a small volume).
    pub grow_declined_at: std::sync::atomic::AtomicU64,
    /// Growth events (`appender_ring_grows`).
    pub ring_grows: std::sync::atomic::AtomicU64,
    /// `(tail, barrier push epoch)` pushed per page write, drained by the
    /// completed barrier that covers them (the DUR-3 discipline per ring).
    pub pending_reclaim: std::sync::Mutex<Vec<(u64, u64)>>,
    /// Conveyor passes inside this region's admit→handoff window (the
    /// Dekker pair with `growing` — growth never swaps a ring a pass is
    /// reserving on).
    pub passes_inside: std::sync::atomic::AtomicUsize,
    /// Stage-B windows of this region between their handoff and their
    /// terminal outcome — counted against growth beside `passes_inside`:
    /// a window's reservation is completed BEFORE its §4.4 pt 4 rollback
    /// runs, and the rollback's compensation reserves on the ring the
    /// window holds, so `drained` alone would let growth swap that ring
    /// out from under it (review round 2, Issue 23).
    pub windows_inflight: std::sync::atomic::AtomicUsize,
    pub growing: std::sync::atomic::AtomicBool,
    /// Woken (`notify_waiters`) when `growing` clears — the pass side
    /// parks on it instead of polling (no poll period to derive).
    pub growth_done: squeezefs_ipc::sqz_notify::Notify,
    /// [`dir_named_mask`] over the directory pair for the CURRENT ring
    /// table — reset to 0 by every table change (a fresh ring at open, a
    /// growth, the leave), raised to [`DIR_NAMED_ALL`] by the page write
    /// that follows ([`page_write_slots`]).
    pub dir_named: std::sync::atomic::AtomicU8,
    /// Recovered from its own `Live` residue at this mount.
    pub self_recovered: bool,
    /// The region's extent grant (§5.3.3) — empty for region 0, whose
    /// images come from the bitmap it owns.
    pub grant: std::sync::Arc<std::sync::Mutex<RegionGrant>>,
    /// The region's SMO rate, milli-images per second (≈ SMOs — an SMO
    /// writes one to `SMO_IMAGES_MAX` fresh images, and an image is what
    /// a grant extent is consumed as), EWMA over checkpoint cycles — the
    /// grant derivation's measured input. Read off the volume's OWN SMO
    /// context (`SmoContext::images_written`, exact per volume), never the
    /// process-wide SMO counters (PR 13g review round 1, Issue 11).
    pub smo_ewma_milli: std::sync::atomic::AtomicU64,
    /// Fresh images this region's slot trees' SMOs wrote since the last
    /// cycle's EWMA fold.
    pub smos_this_cycle: std::sync::atomic::AtomicU64,
    /// The count the last fold consumed (`smo_last_cycle` on the region's
    /// stats) and Σ over every fold (`smo_folded_total`) — the witnesses
    /// for the rate's per-volume input.
    pub smos_last_cycle: std::sync::atomic::AtomicU64,
    pub smos_folded_total: std::sync::atomic::AtomicU64,
    /// The region's COMMIT rate, bytes journaled into its ring per second,
    /// EWMA over checkpoint cycles ([`Self::fold_commit_rate`]) — the ring
    /// derivation's measured input (`appender_ring_bytes_derived`; PR
    /// 13g, F-R5: PR 2 joined every ring at the floor for want of it).
    pub commit_ewma_bytes_per_s: std::sync::atomic::AtomicU64,
    /// The ring head at the last commit-rate fold (positions are bytes;
    /// `u64::MAX` = not yet primed — the first fold measures nothing).
    pub head_at_last_fold: std::sync::atomic::AtomicU64,
    /// Woken when a pass leaves this region's admit→handoff window or a
    /// stage-B window reaches its terminal outcome WHILE `growing` stands
    /// — the drain-then-grow's wait (PR 13g); no poll period to derive.
    pub drained: squeezefs_ipc::sqz_notify::Notify,
    /// SMOs refused for want of a granted extent while the manager could
    /// not refill (`manager_dependency_stalls`, must-stay-0 at the sized
    /// grant).
    pub dependency_stalls: std::sync::atomic::AtomicU64,
    /// Released at the slot-lease cadence (§5.1.3 — its last slot went,
    /// its window was empty): its page is `Free`, its ring returned;
    /// every later cycle skips it.
    pub released: std::sync::atomic::AtomicBool,
}

impl AppenderRegion {
    /// The region's ring right now.
    pub fn ring(&self) -> std::sync::Arc<super::journal::JournalRing> {
        self.ring.load_full()
    }

    /// The region's grant (RAM).
    pub fn grant(&self) -> std::sync::MutexGuard<'_, RegionGrant> {
        self.grant.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fold this cycle's image count into the rate EWMA (`cycle_ms` = the
    /// wall since the last fold): `ewma = ewma × 7/8 + rate / 8`.
    pub fn fold_smo_rate(&self, cycle_ms: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let n = self.smos_this_cycle.swap(0, Relaxed);
        self.smos_last_cycle.store(n, Relaxed);
        self.smos_folded_total.fetch_add(n, Relaxed);
        if cycle_ms == 0 {
            return;
        }
        let rate_milli = n.saturating_mul(1_000_000) / cycle_ms;
        let cur = self.smo_ewma_milli.load(Relaxed);
        self.smo_ewma_milli
            .store(cur - cur / 8 + rate_milli / 8, Relaxed);
    }

    /// Fold the bytes journaled into this region's ring since the last
    /// fold into the commit-rate EWMA (`cycle_ms` = the wall since then):
    /// `ewma = ewma × 7/8 + rate / 8`. The head is a byte position that
    /// continues across a ring swap (`JournalRing::grown_with` keeps it),
    /// so the difference is the cycle's journaled bytes on any table.
    pub fn fold_commit_rate(&self, cycle_ms: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let head = self.ring().core().head();
        let last = self.head_at_last_fold.swap(head, Relaxed);
        if cycle_ms == 0 || last == u64::MAX {
            return;
        }
        let rate = head.saturating_sub(last).saturating_mul(1000) / cycle_ms;
        let cur = self.commit_ewma_bytes_per_s.load(Relaxed);
        self.commit_ewma_bytes_per_s
            .store(cur - cur / 8 + rate / 8, Relaxed);
    }

    /// The drain-then-grow's witnesses (PR 13g): wake a growth waiting
    /// on this region's window to empty — called where a pass leaves the
    /// window and where a stage-B window settles, both under `growing`
    /// (one relaxed read when nothing grows).
    pub fn note_window_left(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        if self.growing.load(SeqCst) {
            self.drained.notify_waiters();
        }
    }

    /// Whether `slot`'s records journal into THIS region's ring.
    pub fn leases_slot(&self, slot: super::record::ForestSlot) -> bool {
        self.leases.load().contains(&slot)
    }

    /// The lease set as of now.
    pub fn leases(&self) -> std::sync::Arc<std::collections::BTreeSet<super::record::ForestSlot>> {
        self.leases.load_full()
    }

    /// Lease `slot` (an acquire / a handover in). The clone-and-store runs
    /// under the set's one-writer mutex (review round 2, Issue 5): a grant
    /// under `manager_verbs` and a release under `handover` on the same
    /// region can never lose each other's update; readers stay lock-free.
    pub fn add_lease(&self, slot: super::record::ForestSlot) {
        let _w = self.leases_writer.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = (**self.leases.load()).clone();
        next.insert(slot);
        self.leases.store(std::sync::Arc::new(next));
    }

    /// Drop `slot`'s lease (a release / a handover out) — under the same
    /// one-writer mutex as [`Self::add_lease`].
    pub fn drop_lease(&self, slot: super::record::ForestSlot) {
        let _w = self.leases_writer.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = (**self.leases.load()).clone();
        next.remove(&slot);
        self.leases.store(std::sync::Arc::new(next));
    }

    /// Growth's release of the Dekker gate: clear `growing`, wake every
    /// pass parked on it.
    pub fn end_growth(&self) {
        self.growing
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.growth_done.notify_waiters();
    }
}

/// The manager-lease posture word per volume (§11): `held` = this mount
/// won the D0 ladder and joined as appender 0; `peer:<node>` = another
/// node's page 0 is `Live`; `vacant` = nobody holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerLease {
    Held,
    Peer { node_token: u64 },
    Vacant,
}

impl ManagerLease {
    /// The stats-inode word.
    pub fn word(&self) -> String {
        match self {
            Self::Held => "held".to_string(),
            Self::Peer { node_token } => format!("peer:{node_token:#018x}"),
            Self::Vacant => "vacant".to_string(),
        }
    }
}

/// The appender regions of one mounted forest volume plus the Appender
/// family's gauges (§11).
pub struct AppenderSet {
    /// Regions in id order; `regions[0]` is this mount's appender 0.
    pub regions: Vec<std::sync::Arc<AppenderRegion>>,
    /// This mount's identity (the writer id is stamped at the join).
    pub identity: AppenderIdentity,
    /// The volume's native routing slot (page entries name routing slots).
    pub native_slot: u16,
    /// `appenders_capacity` (derived: the ring budget over the ring size
    /// an appender joins with).
    pub capacity: u64,
    /// `Live` pages the directory held at mount (foreign ones included —
    /// what a non-writer open reports; a writer refuses on a foreign one).
    pub live_pages_at_mount: u64,
    pub joins: std::sync::atomic::AtomicU64,
    pub leaves: std::sync::atomic::AtomicU64,
    pub self_recoveries: std::sync::atomic::AtomicU64,
    /// **Must-stay-0**: a flush pass that began with dirty leaves of a
    /// region and did not reach its covering barrier within the flush
    /// ceiling (KD-SYM-10).
    pub flush_ceiling_overruns: std::sync::atomic::AtomicU64,
    /// Covering barriers past the landing ceiling but inside `ceiling +
    /// appender_recovery_bound_ms` while a dead appender's recovery held
    /// this volume's SMO mutex (PR 13, defect 33): the recovery's
    /// per-region steps 4–7 run under the mutex the flush pass needs, and
    /// the design's own recovery bound (0.2–0.5 s per region, published)
    /// exceeds the ceiling's 2-tick margin — so a manager leaf dirty when a
    /// recovery begins lands late by a BOUNDED, published amount. Counted
    /// apart so the must-stay-0 overrun keeps its meaning; a barrier past
    /// the extended bound is still an overrun.
    pub flush_ceiling_recovery_extensions: std::sync::atomic::AtomicU64,
    /// Late covering barriers explained by a SERVICE hold of the SMO mutex
    /// (PR 13c, F-B1 — the box's 1–32 ms overruns with no recovery in
    /// flight): the manager's slot grant / release to a wire appender, a
    /// transfer's adoption, a projection refresh, a region's release —
    /// another actor's hold, never this pass waiting on a peer (the
    /// joiner's in-pass wire refill is the pass's own wall). The excluded
    /// time is the MEASURED overlap of such holds with the leaf's dirty
    /// window (`NodeEnv::holds`), capped at ONE landing ceiling
    /// ([`appender_flush_ceiling_service_cap_ms`], published); what
    /// remains past the ceiling is the overrun.
    pub flush_ceiling_service_extensions: std::sync::atomic::AtomicU64,
    /// The Σ of hold time the audit EXCLUDED, ns (recovery and service,
    /// each after its cap), over every leaf it judged — the operator's
    /// face of the exclusion beside the two counts (its delta per
    /// `checkpoints` is the excuse per cycle). Exact-sum.
    pub flush_ceiling_excused_ns: std::sync::atomic::AtomicU64,
    /// The largest single exclusion the audit applied to one leaf, ms —
    /// bounded by `max(recovery bound, service cap)` by construction.
    pub flush_ceiling_excused_max_ms: std::sync::atomic::AtomicU64,
    /// Checkpoint cycles a declared region's ring pressure made due (§4.6
    /// pt 2 per region — [`AppenderSet::ring_pressure`]): a parked
    /// committer is drained by the next cadence tick, never by the
    /// ceiling. 0 on an unpartitioned mount by construction.
    pub pressure_cycles: std::sync::atomic::AtomicU64,
    /// Ring segments a `GrowRing` carved that no page ever named — returned
    /// by the identity's death, leave or clear, or superseded by a stale
    /// pending word at its next ask (`appender_pending_segments_returned`;
    /// PR 13g review round 1, Issue 1 — the detected form of the class
    /// that was a leak no census saw). ≈ 0 on a healthy fleet.
    pub pending_segments_returned: std::sync::atomic::AtomicU64,
    /// Pool extents the own-residue open's census moved back from CLAIMED
    /// to UNCLAIMED (`appender_pool_restored_extents`; PR 13g review round
    /// 1, Issue 2 — the unnamed pool a crash-rejoin's `recover` lands
    /// claimed). 0 on every clean lifecycle.
    pub pool_restored_extents: std::sync::atomic::AtomicU64,
    /// The ring budget's remainder as the last directory read left it
    /// (`appender_ring_budget_remaining_bytes` — the open, every join,
    /// leave and `GrowRing`; Issue 8).
    pub ring_budget_remaining: std::sync::atomic::AtomicU64,
    /// `GrowRing` asks declined because the heap's longest adjacent run
    /// was under the segment floor (`appender_grow_ring_short_declines`;
    /// Issue 9) — the claims released whole, no table slot spent.
    pub grow_ring_short_declines: std::sync::atomic::AtomicU64,
    /// Page-named unclaimed extents a region open DROPPED because the
    /// grant record no longer granted them (`appender_stale_page_words_
    /// dropped`; review round 2, Issue 16a — a word written before a
    /// return landed; the record is the manager's truth). ≈ 0 on a
    /// healthy fleet since the shrink rewrites the page before its return.
    pub stale_page_words_dropped: std::sync::atomic::AtomicU64,
    /// Extents a FRESH join found in its id's `extent_grant` record under
    /// a `Free` page and RETURNED before its own grant
    /// (`appender_join_residue_returned`; review round 2, Issue 14): a
    /// wire `ExtentGrant` carve whose reply was lost — the record is the
    /// witness, the joiner's page word never learnt it, its leave returned
    /// what it knew. ≈ 0 on a healthy fleet.
    pub join_residue_returned: std::sync::atomic::AtomicU64,
    /// `pressure_cycles` as the grant cadence last read it — a cadence
    /// that finds it moved ran on a PRESSURE-DRIVEN cycle (the ring is
    /// the bottleneck, not the heap) and returns nothing (PR 13g, F-R5).
    pub cadence_pressure_seen: std::sync::atomic::AtomicU64,
    /// Set by the writer's JOIN: only a joined mount writes pages (a
    /// guarded offline verb that opens writer-posture never joins).
    pub joined: std::sync::atomic::AtomicBool,
    /// Why the JOIN must refuse, decided at open: a foreign node's `Live`
    /// page or a `Recovering` page (PR 10's recovery driver owns both).
    /// Deferred to the join so the D0 claim gate classifies the volume's
    /// holder first and non-joining Writer opens are never blocked.
    pub join_refusal: Option<String>,
    /// [`appender_flush_ceiling_ms`] of the flush interval in force at
    /// open (the backend-knob convention: resolved once, never per cycle).
    pub flush_ceiling_ms: u64,
    /// [`appender_flush_ceiling_service_cap_ms`] of the same interval —
    /// the published cap on the audit's service exclusion.
    pub flush_ceiling_service_cap_ms: u64,
    /// [`manager_failover_bound_ms`] as derived at this open (the ladder
    /// and replay terms land at the join).
    pub failover_bound_ms: std::sync::atomic::AtomicU64,
    /// The manager-lease posture (decided at open; `Held` once joined).
    pub manager_lease: std::sync::Mutex<ManagerLease>,
    /// The manager holds WERO (rtype 3) on this volume's metadata
    /// namespace (`meta_pr_wero` — 0 = the shipped Write Exclusive, or no
    /// reservation at all).
    pub wero_meta: std::sync::atomic::AtomicBool,
    /// Grants issued by this manager (`extent_grants`) and the extents
    /// they carried (`extent_grant_extents`).
    pub extent_grants: std::sync::atomic::AtomicU64,
    pub extent_grant_extents: std::sync::atomic::AtomicU64,
    /// `ReturnExtents` batches this manager cleared (`extent_returns`).
    pub extent_returns: std::sync::atomic::AtomicU64,
    /// Manager-role releases the §5.5.2 vol-0 rule decided
    /// (`manager_vol0_unreachable`).
    pub vol0_unreachable: std::sync::atomic::AtomicU64,
    /// PR 8: the instant (caller clock ms) volume 0's ledger was first
    /// probed unreachable; 0 = reachable (`note_vol0_ledger_probe`).
    pub vol0_unreachable_since_ms: std::sync::atomic::AtomicU64,
    /// The manager verb ledger (`manager_verb_replays` /
    /// `manager_verb_refusals`, the latter must-stay-0) and the service
    /// phase table (`manager_service_ns`).
    pub verbs: ManagerVerbLedger,
    /// CLOCK_MONOTONIC ns of the last grant-cadence pass (the SMO-rate
    /// EWMA's cycle wall); 0 = none yet.
    pub cadence_last_ns: std::sync::atomic::AtomicU64,
    /// `appenders_known`: the directory's `Live` count as this manager
    /// last read it — in-process regions AND wire joiners — the appender
    /// count the grant cap divides the free heap by (review round 1,
    /// Issue 12). Set at open, raised by a join.
    pub appenders_known: std::sync::atomic::AtomicU64,
    /// The slot-lease plane (design-symmetric-metadata §5.1, PR 4):
    /// `Some` on a writer's open with `SQUEEZEFS_SYMMETRIC_META=1`; `None`
    /// = the PR 1–3 dark forest verbatim.
    pub leases: Option<std::sync::Arc<super::slot_lease::SlotLeasePlane>>,
    /// **A dead recoverer's structure, held for the re-run** (PR 10,
    /// review round 4, Issue 31): the manager's interior records ring 0's
    /// window carried for a slot whose tree-0 lessee's page is
    /// `Recovering` — a recovery in flight whose step-6 flush compacted
    /// the slot's leaves and died before its tree-0 step. The open does
    /// NOT fold them into the slot's tree (the tree is a dead lessee's,
    /// foreign to this mount's flush until the recovery re-runs under the
    /// structural door); it stashes them here, per slot, and the re-run's
    /// step 4 applies them onto the installed page root before the dead
    /// window — a failed re-run puts them back. Their ring-0 positions
    /// stay in the window under the installed root's floor. Empty on
    /// every open that finds no such page.
    pub recovering_structure: std::sync::Mutex<
        std::collections::BTreeMap<super::record::ForestSlot, Vec<RecoveringInterior>>,
    >,
    /// **The joined-appender posture** (design-symmetric-metadata §7.3 /
    /// §5.3, PR 12b): `Some(id)` on a NON-MANAGER RW mount — this mount's
    /// own region is `id` (joined over the wire), and `regions[0]` is the
    /// MANAGER's region read as a PROJECTION (its ring replayed, its page
    /// never written, its ledger / bitmap / tree 0 another process's).
    /// `None` on the manager and on every unarmed / flat mount, where
    /// region 0 is this mount's appender 0 exactly as before.
    pub joined_appender: Option<u32>,
}

/// One stashed interior record (see [`AppenderSet::recovering_structure`]).
#[derive(Debug, Clone)]
pub struct RecoveringInterior {
    pub level: u8,
    /// The record's entry position in ring 0 — its dirty floor when applied.
    pub entry_seq: u64,
    pub record: super::record::Record,
}

/// What the fixed ring's replay WITHHELD for the slots mid-recovery
/// (Issue 31): `slots` = every forest slot tree 0 leases to an appender
/// whose directory page is `Recovering` (any identity — the detector's
/// exemption and the replay's stash filter read the same set); `stash` =
/// the manager's interior records for them, by slot, in the window's
/// order. Handed from the forest replay to the appender open, which
/// applies an OWN region's (own residue, onto the page root it installs)
/// and keeps a foreign one's for the recovery re-run.
#[derive(Debug, Default)]
pub struct RecoveringStructure {
    pub slots: std::collections::BTreeSet<super::record::ForestSlot>,
    pub stash: std::collections::BTreeMap<super::record::ForestSlot, Vec<RecoveringInterior>>,
}

/// Test seam: the manager is UNREACHABLE — the grant cadence issues no
/// refill, so an appender consuming its grant runs out and the
/// `manager_dependency_stalls` arm engages (the in-process face of a
/// dead manager; the failover contracts drive it).
static TEST_MANAGER_UNREACHABLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Arm / disarm the unreachable-manager seam.
pub fn test_set_manager_unreachable(on: bool) {
    TEST_MANAGER_UNREACHABLE.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Whether the seam is armed.
pub fn test_manager_unreachable() -> bool {
    TEST_MANAGER_UNREACHABLE.load(std::sync::atomic::Ordering::SeqCst)
}

/// The manager's verb ledger (§5.3.5 / §11 "Manager family"): every verb
/// answered, the replays answered from DURABLE state (`already`), the
/// refusals (a verb whose durable witness contradicts the caller —
/// must-stay-0), the REJECTIONS (a frame whose wire integers name what
/// the durable state cannot — a run past the volume, wider than the
/// caller's record, an overflowing length: a buggy or hostile peer, kept
/// apart from the witness class so `manager_verb_refusals` keeps its
/// must-stay-0 meaning; review round 1, Issue 2), and the exact-sum
/// service phases `admit / execute / reply / total` with the wall the
/// load percentage is measured against.
#[derive(Debug, Default)]
pub struct ManagerVerbLedger {
    pub verbs: std::sync::atomic::AtomicU64,
    pub replays: std::sync::atomic::AtomicU64,
    pub refusals: std::sync::atomic::AtomicU64,
    pub rejected: std::sync::atomic::AtomicU64,
    /// PR 13 (`extent_grant_conflicts`, **must-stay-0**): grants or
    /// returns REFUSED because an extent already sat in another appender's
    /// `extent_grant:` record — two custodians of one image.
    pub grant_conflicts: std::sync::atomic::AtomicU64,
    /// PR 13 (`extent_return_live_refusals`, **must-stay-0**): extents a
    /// return batch named while a LIVE node of this mount stood at them
    /// — kept claimed, never returned (the double-custody class).
    pub return_live_refusals: std::sync::atomic::AtomicU64,
    /// PR 13g review round 2, Issue 21 (`extent_return_run_cap_refusals`):
    /// `ReturnExtents` batches cut short because freeing the next extent
    /// would SPLIT the caller's grant record past the tree-0 value cap's
    /// run bound (`slot_state::extent_grant_max_runs`) — the record can
    /// name no more fragments. A CAPACITY class: what the record could
    /// name landed, the rest stays granted for the caller's retry; never
    /// the witness class, so `manager_verb_refusals` keeps its
    /// must-stay-0 meaning.
    pub return_run_cap_refusals: std::sync::atomic::AtomicU64,
    /// PR 13 (`extent_grant_stale_page_words`): `ExtentGrant` asks whose
    /// page word named an extent the caller's record no longer held (a
    /// return landed after its last page write) — the word is intersected
    /// with the record, never answered verbatim.
    pub stale_page_words: std::sync::atomic::AtomicU64,
    pub admit_ns: std::sync::atomic::AtomicU64,
    pub execute_ns: std::sync::atomic::AtomicU64,
    pub reply_ns: std::sync::atomic::AtomicU64,
    pub total_ns: std::sync::atomic::AtomicU64,
    /// CLOCK_MONOTONIC ns of the first verb served (the load wall's
    /// origin; 0 = none yet).
    pub first_verb_ns: std::sync::atomic::AtomicU64,
    /// CLOCK_MONOTONIC ns of the last verb served.
    pub last_verb_ns: std::sync::atomic::AtomicU64,
}

impl ManagerVerbLedger {
    /// Record one served verb's phases (ns).
    pub fn record(&self, admit_ns: u64, execute_ns: u64, reply_ns: u64, now_ns: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        self.verbs.fetch_add(1, Relaxed);
        self.admit_ns.fetch_add(admit_ns, Relaxed);
        self.execute_ns.fetch_add(execute_ns, Relaxed);
        self.reply_ns.fetch_add(reply_ns, Relaxed);
        self.total_ns
            .fetch_add(admit_ns + execute_ns + reply_ns, Relaxed);
        let _ = self
            .first_verb_ns
            .compare_exchange(0, now_ns, Relaxed, Relaxed);
        self.last_verb_ns.fetch_max(now_ns, Relaxed);
    }

    /// `manager_verbs_per_s`: verbs over the wall from the first to the
    /// last served (0 with fewer than two).
    pub fn verbs_per_s(&self) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        let first = self.first_verb_ns.load(Relaxed);
        let last = self.last_verb_ns.load(Relaxed);
        if first == 0 || last <= first {
            return 0;
        }
        self.verbs.load(Relaxed).saturating_mul(1_000_000_000) / (last - first)
    }

    /// `manager_load_pct`: Σ service ns ÷ the wall from the first verb to
    /// `now_ns`, in percent — MEASURED, never declared (§5.11).
    pub fn load_pct(&self, now_ns: u64) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        let first = self.first_verb_ns.load(Relaxed);
        if first == 0 || now_ns <= first {
            return 0;
        }
        self.total_ns.load(Relaxed).saturating_mul(100) / (now_ns - first)
    }
}

impl AppenderSet {
    /// Every region's grant-closure terms summed: `(granted, held,
    /// returned, unclaimed)` — the law `extent_grant_extents ≡ claimed +
    /// returned + granted_unclaimed` (§11), `claimed` being what the
    /// regions still HOLD (images, parked frees, unreturned releases).
    pub fn grant_closure(&self) -> (u64, u64, u64, u64) {
        let mut out = (0u64, 0u64, 0u64, 0u64);
        for r in self.regions.iter().skip(1) {
            let g = r.grant();
            out.0 += g.granted;
            out.1 += g.held();
            out.2 += g.returned;
            out.3 += g.unclaimed();
        }
        out
    }

    /// `manager_dependency_stalls` over every region.
    pub fn dependency_stalls(&self) -> u64 {
        self.regions
            .iter()
            .map(|r| {
                r.dependency_stalls
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
            .sum()
    }
    /// The region that journals `slot`'s records: the one leasing it, else
    /// the manager (region 0).
    pub fn region_of_slot(&self, slot: super::record::ForestSlot) -> u32 {
        self.regions
            .iter()
            .skip(1)
            .find(|r| r.leases_slot(slot))
            .map_or(0, |r| r.id)
    }

    /// The region with id `id` (`regions` is id-dense in PR 2: 0 and the
    /// declared ids in order; looked up by id, never by index).
    pub fn region(&self, id: u32) -> Option<&std::sync::Arc<AppenderRegion>> {
        self.regions.iter().find(|r| r.id == id)
    }

    /// Whether region `id` is one THIS MOUNT writes: every region it
    /// holds, minus the manager's projection on a joined appender (PR 12b
    /// — `regions[0]` is then another process's ring, held for its
    /// replay window and its tail, never for a write).
    pub fn owns_region(&self, id: u32) -> bool {
        self.region(id).is_some() && !(id == 0 && self.joined_appender.is_some())
    }

    /// This mount's own appender id: region 0's on the manager, the joined
    /// region's on a non-manager (PR 12b).
    pub fn own_id(&self) -> u32 {
        self.joined_appender.unwrap_or(0)
    }

    /// Whether this mount is a joined NON-MANAGER appender (PR 12b).
    pub fn is_joined_appender(&self) -> bool {
        self.joined_appender.is_some()
    }

    /// Whether more than the manager's region exists.
    pub fn is_partitioned(&self) -> bool {
        self.regions.len() > 1
    }

    /// The lease map (ids ≥ 1) the violation detector reads.
    pub fn lease_map(
        &self,
    ) -> std::collections::BTreeMap<u32, std::collections::BTreeSet<super::record::ForestSlot>>
    {
        self.regions
            .iter()
            .skip(1)
            .map(|r| (r.id, (*r.leases()).clone()))
            .collect()
    }

    /// The slot-lease plane, if armed.
    pub fn slot_leases(&self) -> Option<&std::sync::Arc<super::slot_lease::SlotLeasePlane>> {
        self.leases.as_ref()
    }

    /// §4.6 pt 2's ring-pressure trigger over the DECLARED regions (ring
    /// 0 keeps the shipped law in the tick, byte-identical on a flat
    /// mount): a region's ring is under pressure when its un-reclaimed
    /// distance exceeds half its ADMISSIBLE window. The shipped `distance
    /// > logical_len / 2` is unreachable on a floor-sized ring — the §4.4
    /// pt 5 reserve is half of it — so a full region would never have made
    /// a cycle due and its parked committer waited for the ceiling.
    pub fn ring_pressure(&self) -> bool {
        self.regions.iter().skip(1).any(|r| {
            let ring = r.ring();
            let core = ring.core();
            let geo = core.geometry();
            let admissible = geo.logical_len().saturating_sub(geo.reserve_bytes);
            core.head().saturating_sub(core.reusable_upto()) > admissible / 2
        })
    }

    /// Any declared region's ring holding un-reclaimed entries (`head >
    /// reusable_upto`) — the region face of the tick's "anything to
    /// cover" arm (`distance > 0` on ring 0): a cadence cycle's barrier is
    /// deferred to the NEXT cycle ("reclamation lags a cycle"), so a
    /// region tail sitting in `pending_reclaim` needs one more cycle to
    /// land, and a tick that saw ring 0 idle and nothing dirty would never
    /// run it.
    pub fn rings_uncovered(&self) -> bool {
        self.regions.iter().skip(1).any(|r| {
            let ring = r.ring();
            let core = ring.core();
            core.head() > core.reusable_upto()
        })
    }

    /// `appenders_live`: regions this mount holds — its pages are `Live`,
    /// which is true only after the JOIN (a probe / reader holds region
    /// 0's structure without joining and exports 0, so the closure law
    /// `joins − leaves − recoveries ≡ live` holds on every posture).
    pub fn live(&self) -> u64 {
        if self.joined.load(std::sync::atomic::Ordering::Acquire) {
            // A region RELEASED at the cadence (§5.1.3 — its last slot
            // went) is `Free` again: one leave, no longer live.
            self.own_regions()
                .filter(|r| !r.released.load(std::sync::atomic::Ordering::Acquire))
                .count() as u64
        } else {
            0
        }
    }

    /// The regions THIS MOUNT writes ([`Self::owns_region`]) — every gauge
    /// that reads "this mount's rings / pages" folds over these, so a
    /// joined appender never reports the manager's ring as its own.
    pub fn own_regions(&self) -> impl Iterator<Item = &std::sync::Arc<AppenderRegion>> {
        self.regions.iter().filter(|r| self.owns_region(r.id))
    }

    /// Bytes of ring EXTENTS over every own region (the pages' segment
    /// tables — what the knob sizes; the ring-side ledger pages included).
    pub fn ring_bytes(&self) -> u64 {
        self.own_regions()
            .map(|r| {
                r.page
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .ring_bytes()
            })
            .sum()
    }

    pub fn ring_segments(&self) -> u64 {
        self.own_regions()
            .map(|r| r.ring().segments().len() as u64)
            .sum()
    }

    pub fn ring_grows(&self) -> u64 {
        self.own_regions()
            .map(|r| r.ring_grows.load(std::sync::atomic::Ordering::Relaxed))
            .sum()
    }

    /// The Appender family's snapshot (§11).
    pub fn stats(&self) -> AppenderStats {
        use std::sync::atomic::Ordering::Relaxed;
        let (granted, claimed, returned, unclaimed) = self.grant_closure();
        let now_ns = crate::mono_core::monotonic_ns_u64();
        AppenderStats {
            appender_id: self.own_id(),
            native_slot: self.native_slot,
            live: self.live(),
            live_pages_at_mount: self.live_pages_at_mount,
            capacity: self.capacity,
            joins: self.joins.load(Relaxed),
            leaves: self.leaves.load(Relaxed),
            self_recoveries: self.self_recoveries.load(Relaxed),
            ring_bytes: self.ring_bytes(),
            ring_segments: self.ring_segments(),
            ring_grows: self.ring_grows(),
            flush_ceiling_overruns: self.flush_ceiling_overruns.load(Relaxed),
            flush_ceiling_recovery_extensions: self.flush_ceiling_recovery_extensions.load(Relaxed),
            flush_ceiling_service_extensions: self.flush_ceiling_service_extensions.load(Relaxed),
            flush_ceiling_excused_ns: self.flush_ceiling_excused_ns.load(Relaxed),
            flush_ceiling_excused_max_ms: self.flush_ceiling_excused_max_ms.load(Relaxed),
            flush_ceiling_ms: self.flush_ceiling_ms,
            flush_ceiling_service_cap_ms: self.flush_ceiling_service_cap_ms,
            pressure_cycles: self.pressure_cycles.load(Relaxed),
            pending_segments_returned: self.pending_segments_returned.load(Relaxed),
            pool_restored_extents: self.pool_restored_extents.load(Relaxed),
            ring_budget_remaining_bytes: self.ring_budget_remaining.load(Relaxed),
            grow_ring_short_declines: self.grow_ring_short_declines.load(Relaxed),
            stale_page_words_dropped: self.stale_page_words_dropped.load(Relaxed),
            join_residue_returned: self.join_residue_returned.load(Relaxed),
            manager_lease: self
                .manager_lease
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            meta_pr_wero: self.wero_meta.load(Relaxed),
            failover_bound_ms: self.failover_bound_ms.load(Relaxed),
            dependency_stalls: self.dependency_stalls(),
            extent_grants: self.extent_grants.load(Relaxed),
            extent_grant_extents: self.extent_grant_extents.load(Relaxed),
            extent_returns: self.extent_returns.load(Relaxed),
            grant_granted: granted,
            grant_claimed: claimed,
            grant_returned: returned,
            grant_unclaimed: unclaimed,
            vol0_unreachable: self.vol0_unreachable.load(Relaxed),
            manager_verbs: self.verbs.verbs.load(Relaxed),
            manager_verb_replays: self.verbs.replays.load(Relaxed),
            manager_verb_refusals: self.verbs.refusals.load(Relaxed),
            manager_verb_rejected: self.verbs.rejected.load(Relaxed),
            extent_grant_conflicts: self.verbs.grant_conflicts.load(Relaxed),
            extent_grant_stale_page_words: self.verbs.stale_page_words.load(Relaxed),
            extent_return_live_refusals: self.verbs.return_live_refusals.load(Relaxed),
            extent_return_run_cap_refusals: self.verbs.return_run_cap_refusals.load(Relaxed),
            appenders_known: self.appenders_known.load(Relaxed),
            manager_verbs_per_s: self.verbs.verbs_per_s(),
            manager_load_pct: self.verbs.load_pct(now_ns),
            manager_service_ns: [
                self.verbs.admit_ns.load(Relaxed),
                self.verbs.execute_ns.load(Relaxed),
                self.verbs.reply_ns.load(Relaxed),
                self.verbs.total_ns.load(Relaxed),
            ],
            regions: self
                .regions
                .iter()
                .map(|r| {
                    let ring = r.ring();
                    // `grant` before `page` — the region's lock order.
                    let grant = r.grant();
                    let page = r.page.lock().unwrap_or_else(|e| e.into_inner());
                    AppenderRegionStats {
                        id: r.id,
                        term: page.term,
                        ring_bytes: page.ring_bytes(),
                        segments: ring.segments().len() as u64,
                        ring_entries: ring.written_entries(),
                        stalls: r.stalls.load(Relaxed),
                        leases: r.leases().len() as u64,
                        self_recovered: r.self_recovered,
                        grant_unclaimed: grant.unclaimed(),
                        grant_unclaimed_runs: grant.unclaimed_runs().len() as u64,
                        grant_claimed: grant.claimed(),
                        grant_pending: grant.pending(),
                        grant_returnable: grant.returnable(),
                        grant_promised: grant.promised(),
                        smo_ewma_milli: r.smo_ewma_milli.load(Relaxed),
                        smo_last_cycle: r.smos_last_cycle.load(Relaxed),
                        smo_folded_total: r.smos_folded_total.load(Relaxed),
                        dependency_stalls: r.dependency_stalls.load(Relaxed),
                    }
                })
                .collect(),
        }
    }
}

/// One region's face in [`AppenderStats`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppenderRegionStats {
    pub id: u32,
    pub term: u64,
    pub ring_bytes: u64,
    pub segments: u64,
    /// Journal entries written into this region's ring since open.
    pub ring_entries: u64,
    /// Ring-admission parks on this region's ring.
    pub stalls: u64,
    /// Declared leases (0 for the manager, which leases the complement).
    pub leases: u64,
    pub self_recovered: bool,
    /// The region's grant: unclaimed remainder, claimed images, frees
    /// parked on its tail.
    pub grant_unclaimed: u64,
    /// The unclaimed remainder's runs (the page names ≤ `GRANT_RUNS_MAX`).
    pub grant_unclaimed_runs: u64,
    pub grant_claimed: u64,
    pub grant_pending: u64,
    /// Releases the tail covered, awaiting the cadence's return.
    pub grant_returnable: u64,
    /// Extents the §4.7 admission promised against the grant for
    /// admitted-but-unflushed SMOs of this region's leaves (0 at quiesce).
    pub grant_promised: u64,
    /// The region's SMO rate EWMA (milli-images/s — Issue 11).
    pub smo_ewma_milli: u64,
    /// The image count the last fold consumed, and Σ over every fold
    /// (Issue 11's witnesses).
    pub smo_last_cycle: u64,
    pub smo_folded_total: u64,
    pub dependency_stalls: u64,
}

/// The Appender family (design-symmetric-metadata §11) as one snapshot —
/// `None` on every bit-17-absent mount (`KvMetaBackend::appender_stats`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppenderStats {
    pub appender_id: u32,
    pub native_slot: u16,
    /// Regions this mount holds (`appenders_live`).
    pub live: u64,
    /// `Live` pages the directory held at mount, foreign ones included.
    pub live_pages_at_mount: u64,
    pub capacity: u64,
    pub joins: u64,
    pub leaves: u64,
    pub self_recoveries: u64,
    pub ring_bytes: u64,
    pub ring_segments: u64,
    pub ring_grows: u64,
    pub flush_ceiling_overruns: u64,
    /// Late covering barriers explained by a recovery's SMO-mutex hold
    /// (inside the extended bound) — defect 33.
    pub flush_ceiling_recovery_extensions: u64,
    /// Late covering barriers explained by a service hold of the SMO
    /// mutex (the measured overlap excluded, capped) — PR 13c, F-B1.
    pub flush_ceiling_service_extensions: u64,
    /// Σ excluded hold time, ns (both classes, after their caps).
    pub flush_ceiling_excused_ns: u64,
    /// The largest single exclusion applied to one leaf, ms.
    pub flush_ceiling_excused_max_ms: u64,
    /// The flush ceiling in force, ms (`appender_flush_ceiling_ms`) —
    /// published so the bound the gauge audits cannot drift from the docs.
    pub flush_ceiling_ms: u64,
    /// The service exclusion's cap, ms
    /// (`appender_flush_ceiling_service_cap_ms`).
    pub flush_ceiling_service_cap_ms: u64,
    /// Cycles a declared region's ring pressure made due.
    pub pressure_cycles: u64,
    /// `GrowRing` segments returned unnamed (PR 13g review round 1, Issue 1).
    pub pending_segments_returned: u64,
    /// Pool extents the own-residue census restored (Issue 2).
    pub pool_restored_extents: u64,
    /// The ring budget's remainder (Issue 8).
    pub ring_budget_remaining_bytes: u64,
    /// `GrowRing` asks declined under the segment floor (Issue 9).
    pub grow_ring_short_declines: u64,
    /// Page-named extents a region open dropped as outside the record
    /// (review round 2, Issue 16a).
    pub stale_page_words_dropped: u64,
    /// Record residue a fresh join returned before its grant (review round
    /// 2, Issue 14 — a carve whose reply was lost).
    pub join_residue_returned: u64,
    /// The Manager family (§11, PR 3).
    pub manager_lease: ManagerLease,
    pub meta_pr_wero: bool,
    pub failover_bound_ms: u64,
    pub dependency_stalls: u64,
    pub extent_grants: u64,
    pub extent_grant_extents: u64,
    pub extent_returns: u64,
    /// The grant closure's four terms over every region.
    pub grant_granted: u64,
    pub grant_claimed: u64,
    pub grant_returned: u64,
    pub grant_unclaimed: u64,
    pub vol0_unreachable: u64,
    pub manager_verbs: u64,
    pub manager_verb_replays: u64,
    pub manager_verb_refusals: u64,
    /// Wire-invalid frames rejected before any allocation
    /// (`manager_verb_rejected` — the buggy/hostile-peer class).
    pub manager_verb_rejected: u64,
    /// Grants / returns refused on the grant-disjointness tripwire
    /// (`extent_grant_conflicts`, must-stay-0 — PR 13).
    pub extent_grant_conflicts: u64,
    /// `ExtentGrant` asks whose page word outran the record
    /// (`extent_grant_stale_page_words` — PR 13).
    pub extent_grant_stale_page_words: u64,
    /// Return-batch extents kept claimed because a live node of this mount
    /// stood at them (`extent_return_live_refusals`, must-stay-0 — PR 13).
    pub extent_return_live_refusals: u64,
    /// Return batches cut short at the grant record's run cap — the
    /// CAPACITY class (`extent_return_run_cap_refusals` — PR 13g review
    /// round 2, Issue 21; not the must-stay-0 witness class).
    pub extent_return_run_cap_refusals: u64,
    /// The directory's `Live` count as last read (`appenders_known`) —
    /// the grant cap's appender term.
    pub appenders_known: u64,
    pub manager_verbs_per_s: u64,
    pub manager_load_pct: u64,
    /// `admit / execute / reply / total` ns, exact-sum.
    pub manager_service_ns: [u64; 4],
    pub regions: Vec<AppenderRegionStats>,
}

#[inline]
fn le64(v: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&v[off..off + 8]);
    u64::from_le_bytes(b)
}

#[inline]
fn le32(v: &[u8], off: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&v[off..off + 4]);
    u32::from_le_bytes(b)
}

#[inline]
fn le16(v: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([v[off], v[off + 1]])
}
