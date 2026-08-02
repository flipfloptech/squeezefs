//! Fuzz the v3 **superblock** sector decoder
//! (`src/meta_backend/kv/superblock.rs`).
//!
//! Sector 0 is the first thing a mount reads and the one structure with no
//! second copy to fall back on (DUR-5 is the redundancy item). A torn or
//! tampered superblock must fail LOUD — the §4.10 torn-SB crash case —
//! never panic and never produce a geometry a later reader trusts.
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::superblock::{
    SuperblockV3, FEATURES_INCOMPAT_KNOWN, SUPERBLOCK_V3_LEN,
};

fuzz_target!(|data: &[u8]| {
    // Wrong-length input is its own (cheap) reject arm.
    let _ = SuperblockV3::decode_sector(data);

    let mut sector = vec![0u8; SUPERBLOCK_V3_LEN];
    let n = data.len().min(SUPERBLOCK_V3_LEN);
    sector[..n].copy_from_slice(&data[..n]);

    if let Ok(sb) = SuperblockV3::decode_sector(&sector) {
        // A decoded superblock must re-encode to something that decodes
        // back identically: the geometry a mount trusts is exactly what
        // the writer would produce.
        let re = sb.encode_sector().expect("a decoded superblock re-encodes");
        assert_eq!(re.len(), SUPERBLOCK_V3_LEN);
        let sb2 = SuperblockV3::decode_sector(&re).expect("re-encoded superblock decodes");
        assert_eq!(sb2, sb, "superblock must round-trip");
        assert_eq!(
            sb.features_incompat & !FEATURES_INCOMPAT_KNOWN,
            0,
            "an accepted superblock can never carry unknown incompat bits \
             (forward-only refusal — KD-14)"
        );
    }

    // The KD-14 old-binary path: the SAME sector against a historical
    // known-bits mask must refuse when it carries newer bits, never panic.
    let _ = SuperblockV3::decode_sector_with_known(&sector, 0);
    let _ = SuperblockV3::decode_sector_with_known(&sector, 1);
});
