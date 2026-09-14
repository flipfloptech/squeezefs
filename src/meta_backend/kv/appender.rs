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
const OFF_RESERVED_55: usize = 55; // u8, zero
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
const OFF_SLOTS: usize = OFF_N_SLOTS + 2; // 306

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
    /// `(node_token, mount_slot)` narrowing is PR 10's, where the death
    /// ledger names WHICH of a node's live mounts holds a page; until
    /// then two live mounts of one node on one volume are unrepresentable
    /// (the flock). The writer id changes per mount and never binds.
    pub fn owned_by_node(&self, node_token: u64) -> bool {
        self.node_token == node_token
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
        if buf[OFF_RESERVED_55] != 0
            || buf[OFF_RESERVED_74..OFF_RESERVED_74 + 6]
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
fn page_checksum(img: &[u8]) -> u64 {
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

/// How one page-slot image reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageRead {
    /// A verified page.
    Valid(AppenderPage),
    /// All zero — never written (a fresh pair, or a ring-side slot of an
    /// appender that never checkpointed).
    Blank,
    /// Anything else: torn, foreign bytes, a future layout.
    Corrupt(String),
}

/// Classify one image (total).
pub fn classify_page(buf: &[u8]) -> PageRead {
    if buf.iter().all(|b| *b == 0) {
        return PageRead::Blank;
    }
    match AppenderPage::decode(buf) {
        Ok(p) => PageRead::Valid(p),
        Err(e) => PageRead::Corrupt(e.to_string()),
    }
}

/// Newest-valid-wins over an appender's page-slot images: the valid page
/// with the highest generation and its slot index; `None` when no image
/// verifies (a never-joined appender, or every copy torn).
pub fn newest_valid<B: AsRef<[u8]>>(images: &[B]) -> Option<(usize, AppenderPage)> {
    let mut best: Option<(usize, AppenderPage)> = None;
    for (i, img) in images.iter().enumerate() {
        if let PageRead::Valid(p) = classify_page(img.as_ref()) {
            let newer = best
                .as_ref()
                .is_none_or(|(_, b)| p.generation > b.generation);
            if newer {
                best = Some((i, p));
            }
        }
    }
    best
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

// ---- Extent grants (§5.3.3) -------------------------------------------------

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
    let cap = (free_heap / (4 * appenders.max(1))).max(GRANT_EXTENTS_FLOOR);
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
/// overflow; the first offending run is the rejection, else the number of
/// extents the runs NAME (saturating — a count for the reply, never a
/// capacity). Pure over the integers: nothing here allocates
/// proportional to them (fuzzed by `manager_call_frame`, mirrored in
/// `decoder_property_tests`).
pub fn validate_return_runs(runs: &[GrantRun], total_extents: u64) -> Result<u64, GrantRun> {
    let mut named: u64 = 0;
    for r in runs {
        match r.start.checked_add(u64::from(r.len)) {
            Some(end) if end <= total_extents => {}
            _ => return Err(*r),
        }
        named = named.saturating_add(u64::from(r.len));
    }
    Ok(named)
}

/// The extents `runs` name INSIDE `record`, as an ascending deduplicated
/// list — computed as interval intersections (O(runs × record runs)), so
/// the list is bounded by the record's extent count, never by the frame's
/// integers.
pub fn intersect_runs_with_record(
    runs: &[GrantRun],
    record: &super::slot_state::ExtentGrantRecord,
) -> Vec<u64> {
    let mut extents: Vec<u64> = Vec::new();
    for r in runs {
        let Some(re) = r.start.checked_add(u64::from(r.len)) else {
            continue;
        };
        for g in &record.runs {
            let ge = g.start + u64::from(g.len);
            let (s, e) = (r.start.max(g.start), re.min(ge));
            if s < e {
                extents.extend(s..e);
            }
        }
    }
    extents.sort_unstable();
    extents.dedup();
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
    /// Adopt the runs a grant answered.
    pub fn add_runs(&mut self, runs: &[GrantRun]) {
        for r in runs {
            for e in r.start..r.start + u64::from(r.len) {
                if self.unclaimed.insert(e) {
                    self.granted += 1;
                }
            }
        }
        self.refill_reference = self.unclaimed.len() as u64;
    }

    /// Recovery: the whole grant (tree 0's record) against the page's
    /// unclaimed remainder — everything else is claimed.
    pub fn recover(record_extents: impl IntoIterator<Item = u64>, unclaimed: &[GrantRun]) -> Self {
        let mut g = Self::default();
        for r in unclaimed {
            for e in r.start..r.start + u64::from(r.len) {
                g.unclaimed.insert(e);
            }
        }
        for e in record_extents {
            g.granted += 1;
            if !g.unclaimed.contains(&e) {
                g.claimed.insert(e);
            }
        }
        g.refill_reference = g.unclaimed.len() as u64;
        g
    }

    /// Claim the lowest unclaimed extent.
    pub fn claim(&mut self) -> Option<u64> {
        let e = self.unclaimed.pop_first()?;
        self.claimed.insert(e);
        Some(e)
    }

    /// Recovery fold of an in-window `alloc(extent)` record: the extent is
    /// claimed whatever the page said (the page predates the claim).
    pub fn claim_exact(&mut self, extent: u64) {
        self.unclaimed.remove(&extent);
        self.claimed.insert(extent);
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

    /// The unclaimed remainder as ascending runs — the WHOLE remainder;
    /// the page writer calls [`Self::trim_to_page_runs`] first so the
    /// page names every unclaimed extent (review round 1, Issue 9).
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

    /// Fit the unclaimed remainder to the page's [`GRANT_RUNS_MAX`] runs
    /// WITHOUT truncating it: the largest runs stay unclaimed, every
    /// extent of the rest moves to the returnable batch (the cadence
    /// returns them; a crash-class open recovers exactly the page's
    /// remainder as unclaimed and the returned extents are back in the
    /// heap — never a two-cycle C13 round trip over legitimately
    /// unclaimed extents). Returns how many extents moved.
    pub fn trim_to_page_runs(&mut self) -> u64 {
        let runs = self.unclaimed_runs();
        if runs.len() <= GRANT_RUNS_MAX {
            return 0;
        }
        // Largest first; ties keep the lowest run (a stable sort on a
        // sequence that is ascending by start).
        let mut by_len: Vec<&GrantRun> = runs.iter().collect();
        by_len.sort_by(|a, b| b.len.cmp(&a.len));
        let mut moved = 0u64;
        for r in &by_len[GRANT_RUNS_MAX..] {
            for e in r.start..r.start + u64::from(r.len) {
                if self.unclaimed.remove(&e) {
                    self.returnable.push(e);
                    moved += 1;
                }
            }
        }
        // The 50 % law's reference follows the remainder it measures.
        self.refill_reference = self.refill_reference.saturating_sub(moved);
        moved
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

    /// Drop `extent` from every set that is not a live image: a
    /// `ReturnExtents` of an extent an in-process region's RAM grant still
    /// holds (review round 1, Issue 11) — the bitmap and the RAM grant
    /// must not disagree, or a later `claim()` hands out an extent the
    /// manager re-granted. `Err(())` when the extent is CLAIMED (it holds
    /// an image); `Ok(true)` when it was dropped, `Ok(false)` when no set
    /// held it.
    pub fn drop_returned(&mut self, extent: u64) -> std::result::Result<bool, ()> {
        if self.claimed.contains(&extent) {
            return Err(());
        }
        let mut dropped = self.unclaimed.remove(&extent);
        let before = self.pending.len();
        self.pending.retain(|(e, _)| *e != extent);
        dropped |= self.pending.len() != before;
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
    Ok(newest_valid(&images))
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

fn valid_page(img: &[u8]) -> Option<AppenderPage> {
    match classify_page(img) {
        PageRead::Valid(p) => Some(p),
        PageRead::Blank | PageRead::Corrupt(_) => None,
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

/// Walk the appender directory of a bit-17 volume: appender 0 from the
/// fixed journal extent, appenders ≥ 1 from the chain
/// `superblock.appender_dir` names. Every pair slot of every chain extent
/// is read (a Free/blank pair is an unallocated id). Refuses a chain whose
/// header page does not verify — the directory is a checksummed unit.
pub async fn read_directory(
    path: &Path,
    sb: &super::superblock::SuperblockV3,
) -> Result<Vec<AppenderEntry>, KvError> {
    let mut out = Vec::new();
    // Appender 0: all four slots sit in the fixed extent.
    let offs0 = appender0_page_offsets(&sb.journal);
    let mut images0 = Vec::with_capacity(offs0.len());
    for off in &offs0 {
        images0.push(read_page(path, *off).await?);
    }
    out.push(AppenderEntry {
        appender_id: 0,
        dir_offsets: [offs0[0], offs0[1]],
        page: newest_valid(&images0).map(|(_, p)| p),
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
            if let Some((_, p)) = newest_valid(&images) {
                if let Some(first) = p.segments.first() {
                    for off in ring_side_offsets(first) {
                        images.push(read_page(path, off).await?);
                    }
                }
            }
            out.push(AppenderEntry {
                appender_id: next_id,
                dir_offsets: offs,
                page: newest_valid(&images).map(|(_, p)| p),
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
    /// Forest slots this region leases (the declared partition); empty
    /// for the manager, which leases the complement.
    pub leases: std::collections::BTreeSet<super::record::ForestSlot>,
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
    /// The region's SMO rate, milli-SMOs per second, EWMA over checkpoint
    /// cycles — the grant derivation's measured input.
    pub smo_ewma_milli: std::sync::atomic::AtomicU64,
    /// SMOs of this region's slot trees since the last cycle's EWMA fold.
    pub smos_this_cycle: std::sync::atomic::AtomicU64,
    /// SMOs refused for want of a granted extent while the manager could
    /// not refill (`manager_dependency_stalls`, must-stay-0 at the sized
    /// grant).
    pub dependency_stalls: std::sync::atomic::AtomicU64,
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

    /// Fold this cycle's SMO count into the rate EWMA (`cycle_ms` = the
    /// wall since the last fold): `ewma = ewma × 7/8 + rate / 8`.
    pub fn fold_smo_rate(&self, cycle_ms: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let n = self.smos_this_cycle.swap(0, Relaxed);
        if cycle_ms == 0 {
            return;
        }
        let rate_milli = n.saturating_mul(1_000_000) / cycle_ms;
        let cur = self.smo_ewma_milli.load(Relaxed);
        self.smo_ewma_milli
            .store(cur - cur / 8 + rate_milli / 8, Relaxed);
    }

    /// Whether `slot`'s records journal into THIS region's ring.
    pub fn leases_slot(&self, slot: super::record::ForestSlot) -> bool {
        self.leases.contains(&slot)
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
    /// Checkpoint cycles a declared region's ring pressure made due (§4.6
    /// pt 2 per region — [`AppenderSet::ring_pressure`]): a parked
    /// committer is drained by the next cadence tick, never by the
    /// ceiling. 0 on an unpartitioned mount by construction.
    pub pressure_cycles: std::sync::atomic::AtomicU64,
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

    /// Whether more than the manager's region exists.
    pub fn is_partitioned(&self) -> bool {
        self.regions.len() > 1
    }

    /// The declared lease map (ids ≥ 1) the violation detector reads.
    pub fn lease_map(
        &self,
    ) -> std::collections::BTreeMap<u32, std::collections::BTreeSet<super::record::ForestSlot>>
    {
        self.regions
            .iter()
            .skip(1)
            .map(|r| (r.id, r.leases.clone()))
            .collect()
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
            self.regions.len() as u64
        } else {
            0
        }
    }

    /// Bytes of ring EXTENTS over every region (the pages' segment
    /// tables — what the knob sizes; the ring-side ledger pages included).
    pub fn ring_bytes(&self) -> u64 {
        self.regions
            .iter()
            .map(|r| {
                r.page
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .ring_bytes()
            })
            .sum()
    }

    pub fn ring_segments(&self) -> u64 {
        self.regions
            .iter()
            .map(|r| r.ring().segments().len() as u64)
            .sum()
    }

    pub fn ring_grows(&self) -> u64 {
        self.regions
            .iter()
            .map(|r| r.ring_grows.load(std::sync::atomic::Ordering::Relaxed))
            .sum()
    }

    /// The Appender family's snapshot (§11).
    pub fn stats(&self) -> AppenderStats {
        use std::sync::atomic::Ordering::Relaxed;
        let (granted, claimed, returned, unclaimed) = self.grant_closure();
        let now_ns = crate::mono_core::monotonic_ns_u64();
        AppenderStats {
            appender_id: self.regions.first().map_or(0, |r| r.id),
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
            flush_ceiling_ms: self.flush_ceiling_ms,
            pressure_cycles: self.pressure_cycles.load(Relaxed),
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
                    let page = r.page.lock().unwrap_or_else(|e| e.into_inner());
                    let grant = r.grant();
                    AppenderRegionStats {
                        id: r.id,
                        term: page.term,
                        ring_bytes: page.ring_bytes(),
                        segments: ring.segments().len() as u64,
                        ring_entries: ring.written_entries(),
                        stalls: r.stalls.load(Relaxed),
                        leases: r.leases.len() as u64,
                        self_recovered: r.self_recovered,
                        grant_unclaimed: grant.unclaimed(),
                        grant_claimed: grant.claimed(),
                        grant_pending: grant.pending(),
                        grant_returnable: grant.returnable(),
                        grant_promised: grant.promised(),
                        smo_ewma_milli: r.smo_ewma_milli.load(Relaxed),
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
    pub grant_claimed: u64,
    pub grant_pending: u64,
    /// Releases the tail covered, awaiting the cadence's return.
    pub grant_returnable: u64,
    /// Extents the §4.7 admission promised against the grant for
    /// admitted-but-unflushed SMOs of this region's leaves (0 at quiesce).
    pub grant_promised: u64,
    /// The region's SMO rate EWMA (milli-SMOs/s).
    pub smo_ewma_milli: u64,
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
    /// The flush ceiling in force, ms (`appender_flush_ceiling_ms`) —
    /// published so the bound the gauge audits cannot drift from the docs.
    pub flush_ceiling_ms: u64,
    /// Cycles a declared region's ring pressure made due.
    pub pressure_cycles: u64,
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
