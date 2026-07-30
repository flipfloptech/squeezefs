//! Layout wire formats — the persisted `"layout"` xattr value and its
//! **delta record** (the 2026-07-30 write-commit-economy campaign,
//! `.benchmarks/2026-07-30-write-commit-economy.md`).
//!
//! ## Why this module exists
//!
//! The block-publish hot path used to re-serialize the ENTIRE
//! [`LayoutMetadata`] (block map included) into every journal entry and
//! every node writeback — an O(file_size)-bytes-per-publish term the
//! 2026-07-30 meta-plane audit convicted as the standing write ceiling
//! (18.2 KiB mean journal entry field-wide; `.benchmarks/2026-07-30-
//! meta-plane-writes.md` §3). The fix is a `Delta`-kind record on the
//! layout xattr key carrying ONLY the changed `(block → key)` entries
//! plus the (tiny) absolute non-map fields; the §4.2 fold algebra
//! reconstructs the full value exactly as it does for [`InodeDelta`]
//! Δtime records.
//!
//! [`InodeDelta`]: crate::meta_backend::kv::record::InodeDelta
//!
//! ## The divergence-proof shape: "full minus map"
//!
//! A [`LayoutDelta`] carries every [`LayoutMetadata`] field EXCEPT the
//! block map **absolutely** (`file_type`, `size`, `block_map_id`,
//! `block_prefix`, `file_id`, `data_key`), and merges only its `entries`
//! into the base's map. This confines the only state the fold inherits
//! from the base to the block map itself — which is mutated exclusively
//! through the persisting merge paths (`merge_block_mappings*` under
//! `INODE_META_LOCKS`, every one of which saves), so the RAM authority
//! and the fold-reconstructed value cannot diverge on any other field
//! (staged-identity flips, deferred size floors, and file-type
//! promotions all ride the delta verbatim).
//!
//! ## Refusals (writer rules made structural)
//!
//! `apply` refuses loud (never guesses) when the base is not an inline
//! bincode layout: an `indirect:` base (its map lives in a data-plane
//! blob — inserting inline entries would shadow it) or an undecodable /
//! legacy-JSON base. The writer-side eligibility ladder
//! (`CachedMetadata::layout_delta_chain`) makes these unreachable in a
//! healthy volume; hitting one at fold time is genuine corruption.
//!
//! ## Canonical (deterministic) encoding
//!
//! [`LayoutMetadata`]'s block map serializes in **ascending block-index
//! order** (`serialize_with`), so folds that materialize a layout value
//! are byte-deterministic: replay-twice digest equality and the
//! `fold_forward ≡ fold_newest_first` property hold at the byte level.
//! Decoding is unchanged (bincode map decode accepts any order), so
//! pre-campaign stored values remain readable verbatim.

use serde::ser::{SerializeMap, Serializer};
use std::collections::HashMap;

/// The persisted `"layout"` xattr value (bincode). Moved here from
/// `routing.rs` (re-exported there) so the KV fold layer can decode /
/// re-encode it without depending on the FUSE routing module.
#[derive(serde::Serialize, serde::Deserialize, Clone, Default, Debug)]
pub struct LayoutMetadata {
    pub file_type: String,
    pub size: u64,
    pub block_map_id: Option<String>,
    pub block_prefix: Option<String>,
    pub file_id: Option<String>,
    pub data_key: Option<Vec<u8>>,
    /// Canonical order on the wire (see module docs) — decode-compatible
    /// with every historically stored value.
    #[serde(serialize_with = "serialize_block_map_sorted")]
    pub block_map: Option<HashMap<u32, String>>,
}

/// Serialize `Option<HashMap<u32, String>>` with entries in ascending
/// block-index order — byte-compatible with bincode's default
/// `Option<HashMap>` encoding (tag + len + pairs), deterministic.
fn serialize_block_map_sorted<S: Serializer>(
    map: &Option<HashMap<u32, String>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    struct Sorted<'a>(&'a HashMap<u32, String>);
    impl serde::Serialize for Sorted<'_> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            let mut entries: Vec<(&u32, &String)> = self.0.iter().collect();
            entries.sort_unstable_by_key(|(b, _)| **b);
            let mut m = s.serialize_map(Some(entries.len()))?;
            for (b, k) in entries {
                m.serialize_entry(b, k)?;
            }
            m.end()
        }
    }
    match map {
        None => s.serialize_none(),
        Some(m) => s.serialize_some(&Sorted(m)),
    }
}

/// Leading `u16` (LE) of a layout delta payload. Deliberately outside
/// `InodeDelta`'s valid mask space (`mask & !DELTA_MASK_ALL != 0`), so
/// the shared fold can branch on the payload alone and a pre-campaign
/// binary's `InodeDelta::decode` rejects it loud instead of misfolding.
/// (Old binaries never get that far: volumes carrying layout deltas are
/// stamped with the `KV_LAYOUT_DELTAS` incompat bit and refuse to mount.)
pub const LAYOUT_DELTA_MAGIC: u16 = 0x4C31; // "L1"

/// `true` iff `payload` is a layout delta (magic peek) — the fold's
/// branch discriminator. A payload shorter than the magic is nobody's
/// delta and falls through to the inode decoder's loud rejection.
pub fn is_layout_delta(payload: &[u8]) -> bool {
    payload.len() >= 2 && u16::from_le_bytes([payload[0], payload[1]]) == LAYOUT_DELTA_MAGIC
}

/// Layout-wire errors. Mapped to `KvError::Corrupt` by the fold layer.
#[derive(Debug, PartialEq, Eq)]
pub enum LayoutWireError {
    /// Truncated / trailing bytes / lying length fields.
    Malformed(String),
    /// The base value the delta folds onto is not an inline bincode
    /// layout (indirect, legacy-JSON, or undecodable) — writer rules
    /// forbid staging a delta on such a base, so this is corruption.
    BadBase(String),
}

impl std::fmt::Display for LayoutWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutWireError::Malformed(s) => write!(f, "layout delta malformed: {s}"),
            LayoutWireError::BadBase(s) => write!(f, "layout delta base unusable: {s}"),
        }
    }
}

impl std::error::Error for LayoutWireError {}

/// A block-publish layout delta: absolute non-map fields + `(block →
/// key)` map inserts (see module docs "full minus map"). One delta
/// carries one coalesced publish batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutDelta {
    pub file_type: String,
    pub size: u64,
    pub block_map_id: Option<String>,
    pub block_prefix: Option<String>,
    pub file_id: Option<String>,
    pub data_key: Option<Vec<u8>>,
    /// Map inserts, `(block_index, block_key)`. Inserts only — removals
    /// (truncate/punch) take the full-`Put` path by design.
    pub entries: Vec<(u32, String)>,
}

const F_BLOCK_MAP_ID: u8 = 1 << 0;
const F_BLOCK_PREFIX: u8 = 1 << 1;
const F_FILE_ID: u8 = 1 << 2;
const F_DATA_KEY: u8 = 1 << 3;
const F_KNOWN: u8 = F_BLOCK_MAP_ID | F_BLOCK_PREFIX | F_FILE_ID | F_DATA_KEY;

impl LayoutDelta {
    /// Build the delta representing `final_state` (the post-merge
    /// layout, block map excluded) with `entries` as the batch's map
    /// inserts.
    pub fn from_final_state(
        file_type: &str,
        size: u64,
        block_map_id: Option<&str>,
        block_prefix: Option<&str>,
        file_id: Option<&str>,
        data_key: Option<&[u8]>,
        entries: Vec<(u32, String)>,
    ) -> Self {
        Self {
            file_type: file_type.to_string(),
            size,
            block_map_id: block_map_id.map(str::to_string),
            block_prefix: block_prefix.map(str::to_string),
            file_id: file_id.map(str::to_string),
            data_key: data_key.map(<[u8]>::to_vec),
            entries,
        }
    }

    /// Encode (little-endian, strict):
    /// `magic u16 | flags u8 | file_type_len u8 | file_type | size u64 |
    /// [block_map_id u16+bytes] | [block_prefix u16+bytes] |
    /// [file_id u16+bytes] | [data_key u16+bytes] | entry_count u32 |
    /// entries × { block u32 | key_len u16 | key }`.
    pub fn encode(&self) -> Vec<u8> {
        debug_assert!(self.file_type.len() <= u8::MAX as usize);
        let mut flags = 0u8;
        if self.block_map_id.is_some() {
            flags |= F_BLOCK_MAP_ID;
        }
        if self.block_prefix.is_some() {
            flags |= F_BLOCK_PREFIX;
        }
        if self.file_id.is_some() {
            flags |= F_FILE_ID;
        }
        if self.data_key.is_some() {
            flags |= F_DATA_KEY;
        }
        let mut out = Vec::with_capacity(
            32 + self.file_type.len()
                + self.entries.iter().map(|(_, k)| 6 + k.len()).sum::<usize>(),
        );
        out.extend_from_slice(&LAYOUT_DELTA_MAGIC.to_le_bytes());
        out.push(flags);
        out.push(self.file_type.len() as u8);
        out.extend_from_slice(self.file_type.as_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        let mut opt = |bytes: Option<&[u8]>| {
            if let Some(b) = bytes {
                debug_assert!(b.len() <= u16::MAX as usize);
                out.extend_from_slice(&(b.len() as u16).to_le_bytes());
                out.extend_from_slice(b);
            }
        };
        opt(self.block_map_id.as_deref().map(str::as_bytes));
        opt(self.block_prefix.as_deref().map(str::as_bytes));
        opt(self.file_id.as_deref().map(str::as_bytes));
        opt(self.data_key.as_deref());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for (b, k) in &self.entries {
            debug_assert!(k.len() <= u16::MAX as usize);
            out.extend_from_slice(&b.to_le_bytes());
            out.extend_from_slice(&(k.len() as u16).to_le_bytes());
            out.extend_from_slice(k.as_bytes());
        }
        out
    }

    /// Decode; rejects a wrong magic, unknown flags, truncation, lying
    /// lengths, non-UTF-8 strings, and trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, LayoutWireError> {
        let mut r = Rd { b: bytes, off: 0 };
        let magic = r.u16("magic")?;
        if magic != LAYOUT_DELTA_MAGIC {
            return Err(LayoutWireError::Malformed(format!(
                "bad magic {magic:#06x} (want {LAYOUT_DELTA_MAGIC:#06x})"
            )));
        }
        let flags = r.u8("flags")?;
        if flags & !F_KNOWN != 0 {
            return Err(LayoutWireError::Malformed(format!(
                "unknown flag bits {flags:#04x}"
            )));
        }
        let ft_len = r.u8("file_type len")? as usize;
        let file_type = r.str_exact(ft_len, "file_type")?;
        let size = r.u64("size")?;
        let mut opt_str = |name: &str, bit: u8| -> Result<Option<String>, LayoutWireError> {
            if flags & bit == 0 {
                return Ok(None);
            }
            let len = r.u16(name)? as usize;
            Ok(Some(r.str_exact(len, name)?))
        };
        let block_map_id = opt_str("block_map_id", F_BLOCK_MAP_ID)?;
        let block_prefix = opt_str("block_prefix", F_BLOCK_PREFIX)?;
        let file_id = opt_str("file_id", F_FILE_ID)?;
        let data_key = if flags & F_DATA_KEY != 0 {
            let len = r.u16("data_key")? as usize;
            Some(r.take(len, "data_key")?.to_vec())
        } else {
            None
        };
        let n = r.u32("entry_count")? as usize;
        let mut entries = Vec::with_capacity(n.min(4096));
        for i in 0..n {
            let b = r.u32("entry block")?;
            let klen = r.u16("entry key len")? as usize;
            let k = r.str_exact(klen, "entry key")?;
            entries.push((b, k));
            let _ = i;
        }
        r.finish("layout delta")?;
        Ok(Self {
            file_type,
            size,
            block_map_id,
            block_prefix,
            file_id,
            data_key,
            entries,
        })
    }

    /// Fold this delta onto `base` (a bincode [`LayoutMetadata`] — the
    /// raw xattr VALUE bytes, not the XattrValue envelope) and return
    /// the folded value canonically re-encoded. Refuses non-inline
    /// bases loud (module docs "Refusals").
    pub fn apply(&self, base: &[u8]) -> Result<Vec<u8>, LayoutWireError> {
        if base.first() == Some(&b'{') {
            return Err(LayoutWireError::BadBase(
                "legacy JSON layout base — deltas require a bincode base".into(),
            ));
        }
        let mut layout: LayoutMetadata = bincode::deserialize(base).map_err(|e| {
            LayoutWireError::BadBase(format!("base does not decode as a bincode layout: {e}"))
        })?;
        if layout
            .block_map_id
            .as_deref()
            .is_some_and(|id| id.starts_with("indirect:"))
        {
            return Err(LayoutWireError::BadBase(
                "indirect base — its map lives in a data-plane blob; a delta cannot fold onto it"
                    .into(),
            ));
        }
        self.apply_to(&mut layout);
        bincode::serialize(&layout)
            .map_err(|e| LayoutWireError::Malformed(format!("re-encode failed: {e}")))
    }

    /// The in-place fold: map inserts + absolute non-map overwrite.
    pub fn apply_to(&self, layout: &mut LayoutMetadata) {
        if !self.entries.is_empty() {
            let map = layout.block_map.get_or_insert_with(HashMap::new);
            for (b, k) in &self.entries {
                map.insert(*b, k.clone());
            }
        }
        layout.file_type = self.file_type.clone();
        layout.size = self.size;
        layout.block_map_id = self.block_map_id.clone();
        layout.block_prefix = self.block_prefix.clone();
        layout.file_id = self.file_id.clone();
        layout.data_key = self.data_key.clone();
    }
}

/// Minimal strict reader (the `record.rs` `Reader` shape, local so this
/// module stays dependency-free of the KV layer).
struct Rd<'a> {
    b: &'a [u8],
    off: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], LayoutWireError> {
        if self.off + n > self.b.len() {
            return Err(LayoutWireError::Malformed(format!(
                "truncated at {what} (need {n} B at offset {}, have {})",
                self.off,
                self.b.len()
            )));
        }
        let s = &self.b[self.off..self.off + n];
        self.off += n;
        Ok(s)
    }
    fn u8(&mut self, what: &str) -> Result<u8, LayoutWireError> {
        Ok(self.take(1, what)?[0])
    }
    fn u16(&mut self, what: &str) -> Result<u16, LayoutWireError> {
        Ok(u16::from_le_bytes(self.take(2, what)?.try_into().unwrap()))
    }
    fn u32(&mut self, what: &str) -> Result<u32, LayoutWireError> {
        Ok(u32::from_le_bytes(self.take(4, what)?.try_into().unwrap()))
    }
    fn u64(&mut self, what: &str) -> Result<u64, LayoutWireError> {
        Ok(u64::from_le_bytes(self.take(8, what)?.try_into().unwrap()))
    }
    fn str_exact(&mut self, n: usize, what: &str) -> Result<String, LayoutWireError> {
        String::from_utf8(self.take(n, what)?.to_vec())
            .map_err(|_| LayoutWireError::Malformed(format!("{what} is not UTF-8")))
    }
    fn finish(&self, what: &str) -> Result<(), LayoutWireError> {
        if self.off != self.b.len() {
            return Err(LayoutWireError::Malformed(format!(
                "{what}: {} trailing bytes",
                self.b.len() - self.off
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LayoutDelta {
        LayoutDelta {
            file_type: "striped".into(),
            size: 12 * 1024 * 1024,
            block_map_id: Some("block_map_42".into()),
            block_prefix: None,
            file_id: Some("stage-abc".into()),
            data_key: Some(vec![9, 8, 7]),
            entries: vec![(0, "b0://0".into()), (2, "b0://8388608".into())],
        }
    }

    #[test]
    fn roundtrip_and_strictness() {
        let d = sample();
        let bytes = d.encode();
        assert!(is_layout_delta(&bytes));
        assert_eq!(LayoutDelta::decode(&bytes).expect("roundtrip"), d);
        // Truncation and trailing bytes reject.
        assert!(LayoutDelta::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(LayoutDelta::decode(&trailing).is_err());
        // Wrong magic rejects (and is not a layout delta).
        let mut wrong = bytes;
        wrong[0] ^= 0xFF;
        assert!(!is_layout_delta(&wrong));
        assert!(LayoutDelta::decode(&wrong).is_err());
    }

    #[test]
    fn canonical_block_map_encoding_is_deterministic() {
        let mut m1 = HashMap::new();
        let mut m2 = HashMap::new();
        // Insert in different orders; encodings must be identical.
        for b in 0..64u32 {
            m1.insert(b, format!("k{b}"));
        }
        for b in (0..64u32).rev() {
            m2.insert(b, format!("k{b}"));
        }
        let l1 = LayoutMetadata {
            file_type: "striped".into(),
            size: 1,
            block_map: Some(m1),
            ..Default::default()
        };
        let mut l2 = l1.clone();
        l2.block_map = Some(m2);
        assert_eq!(
            bincode::serialize(&l1).unwrap(),
            bincode::serialize(&l2).unwrap(),
            "block map serialization must be order-canonical"
        );
        // And decode-compatible.
        let rt: LayoutMetadata =
            bincode::deserialize(&bincode::serialize(&l1).unwrap()).expect("decode");
        assert_eq!(rt.block_map.as_ref().unwrap().len(), 64);
    }

    #[test]
    fn apply_refuses_indirect_and_json_and_garbage_bases() {
        let d = sample();
        let indirect = LayoutMetadata {
            file_type: "striped".into(),
            size: 4,
            block_map_id: Some("indirect:b0://123".into()),
            ..Default::default()
        };
        let base = bincode::serialize(&indirect).unwrap();
        assert!(matches!(d.apply(&base), Err(LayoutWireError::BadBase(_))));
        assert!(matches!(
            d.apply(br#"{"file_type":"striped"}"#),
            Err(LayoutWireError::BadBase(_))
        ));
        assert!(d.apply(&[0xFF; 3]).is_err());
    }

    #[test]
    fn apply_merges_entries_and_overwrites_non_map_fields() {
        let base = LayoutMetadata {
            file_type: "staged".into(),
            size: 100,
            block_map_id: None,
            block_prefix: Some("keepme-not".into()),
            file_id: Some("stage-old".into()),
            data_key: None,
            block_map: Some(HashMap::from([(1, "old1".to_string())])),
        };
        let bytes = bincode::serialize(&base).unwrap();
        let d = sample();
        let folded = d.apply(&bytes).expect("apply");
        let got: LayoutMetadata = bincode::deserialize(&folded).unwrap();
        assert_eq!(got.file_type, "striped");
        assert_eq!(got.size, d.size);
        assert_eq!(got.block_map_id.as_deref(), Some("block_map_42"));
        assert_eq!(got.block_prefix, None, "absolute fields overwrite");
        assert_eq!(got.file_id.as_deref(), Some("stage-abc"));
        assert_eq!(got.data_key.as_deref(), Some(&[9u8, 8, 7][..]));
        let map = got.block_map.expect("map");
        assert_eq!(map.len(), 3, "insert-merge keeps unnamed entries");
        assert_eq!(map[&1], "old1");
        assert_eq!(map[&0], "b0://0");
        assert_eq!(map[&2], "b0://8388608");
        // Deterministic output.
        assert_eq!(folded, d.apply(&bytes).unwrap());
    }
}
