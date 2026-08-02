//! Fuzz every v3 **record-level** decoder (`src/meta_backend/kv/record.rs`).
//!
//! These run on bytes that already passed a container checksum, so their
//! job is defence-in-depth against writer bugs and against an attacker who
//! can re-stamp a digest. None of them may panic, and `decode(encode(x))`
//! must round-trip for the ones that encode (spec §11 TEST-4).
#![no_main]

use libfuzzer_sys::fuzz_target;
use squeezefs::meta_backend::kv::record::{
    decode_dentry_key, decode_inode_key, decode_readdir_cookie, decode_xattr_key, DentryValue,
    InodeDelta, InodeValue, RecordRef, XattrValue,
};

fuzz_target!(|data: &[u8]| {
    // --- keys ---------------------------------------------------------
    let _ = decode_inode_key(data);
    let _ = decode_dentry_key(data);
    let _ = decode_xattr_key(data);
    if data.len() >= 8 {
        let cookie = u64::from_le_bytes(data[..8].try_into().unwrap());
        let _ = decode_readdir_cookie(cookie);
    }

    // --- values: never panic, and round-trip whatever decodes ---------
    if let Ok(v) = InodeValue::decode(data) {
        let re = v.encode();
        assert_eq!(
            InodeValue::decode(&re).ok(),
            Some(v),
            "InodeValue must round-trip"
        );
    }
    if let Ok(v) = DentryValue::decode(data) {
        let re = v.encode().expect("a decoded dentry re-encodes");
        assert_eq!(
            DentryValue::decode(&re).ok(),
            Some(v),
            "DentryValue must round-trip"
        );
    }
    if let Ok(v) = XattrValue::decode(data) {
        let re = v.encode().expect("a decoded xattr re-encodes");
        assert_eq!(
            XattrValue::decode(&re).ok(),
            Some(v),
            "XattrValue must round-trip"
        );
    }
    if let Ok(d) = InodeDelta::decode(data) {
        let re = d.encode();
        assert_eq!(
            InodeDelta::decode(&re).ok(),
            Some(d),
            "InodeDelta must round-trip"
        );
    }

    // --- the record frame ---------------------------------------------
    let mut pos = 0usize;
    let mut guard = 0u32;
    while pos < data.len() {
        // A decoder that reports `used == 0` would spin forever here; that
        // is itself a bug worth catching.
        guard += 1;
        assert!(guard < 100_000, "record walk failed to make progress");
        match RecordRef::decode(&data[pos..]) {
            Ok((r, used)) => {
                assert!(used > 0, "a decoded record must consume bytes");
                assert!(pos + used <= data.len(), "record overran its container");
                assert!(!r.key.is_empty(), "empty keys must be rejected");
                let mut re = Vec::new();
                r.encode_into(&mut re);
                let (r2, used2) = RecordRef::decode(&re).expect("re-encoded record decodes");
                assert_eq!((r2.key, r2.seq, r2.value), (r.key, r.seq, r.value));
                assert_eq!(used2, re.len(), "encode/decode lengths agree");
                pos += used;
            }
            Err(_) => break,
        }
    }
});
