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
    let page0 = read_newest_page(path, &offs0).await?.map(|(_, p)| p);
    out.push(AppenderEntry {
        appender_id: 0,
        dir_offsets: [offs0[0], offs0[1]],
        page: page0,
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
            });
            next_id += 1;
        }
        extent = hdr.next;
        chain_index += 1;
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
/// and barriered — within this many ms. It IS the checkpoint cadence
/// ceiling (`CHECKPOINT_MAX_AGE_MS`), stated once; the reader's landing
/// ceiling adds its tick terms on top of it, never the other way round.
pub fn appender_flush_ceiling_ms() -> u64 {
    super::checkpoint::CHECKPOINT_MAX_AGE_MS as u64
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
    pub growing: std::sync::atomic::AtomicBool,
    /// Recovered from its own `Live` residue at this mount.
    pub self_recovered: bool,
}

impl AppenderRegion {
    /// The region's ring right now.
    pub fn ring(&self) -> std::sync::Arc<super::journal::JournalRing> {
        self.ring.load_full()
    }

    /// Whether `slot`'s records journal into THIS region's ring.
    pub fn leases_slot(&self, slot: super::record::ForestSlot) -> bool {
        self.leases.contains(&slot)
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
}

impl AppenderSet {
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

    /// `appenders_live`: regions this mount holds (its pages are `Live`).
    pub fn live(&self) -> u64 {
        self.regions.len() as u64
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
            pressure_cycles: self.pressure_cycles.load(Relaxed),
            regions: self
                .regions
                .iter()
                .map(|r| {
                    let ring = r.ring();
                    let page = r.page.lock().unwrap_or_else(|e| e.into_inner());
                    AppenderRegionStats {
                        id: r.id,
                        term: page.term,
                        ring_bytes: page.ring_bytes(),
                        segments: ring.segments().len() as u64,
                        ring_entries: ring.written_entries(),
                        stalls: r.stalls.load(Relaxed),
                        leases: r.leases.len() as u64,
                        self_recovered: r.self_recovered,
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
    /// Cycles a declared region's ring pressure made due.
    pub pressure_cycles: u64,
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
