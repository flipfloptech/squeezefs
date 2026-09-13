//! Offline, read-only archaeology over copied user meta volumes (multi-volume
//! EIO investigation). Run explicitly:
//!   MV_EIO_META="/path/mds1,/path/mds2,..." cargo test --test mv_eio_archaeology -- --ignored --nocapture
#![allow(clippy::type_complexity)]

use std::collections::HashMap;

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn walk_user_meta_key_forms() {
    let Ok(paths) = std::env::var("MV_EIO_META") else {
        squeezefs_testkit::skip!(OptIn, "MV_EIO_META not set");
    };
    let mut per_prefix: HashMap<String, u64> = HashMap::new();
    // (be_id, offset) -> count of referencing (vol, ino, block)
    let mut owners: HashMap<(String, u64), Vec<(usize, u64, u32)>> = HashMap::new();
    let mut total_layouts = 0u64;
    let mut striped = 0u64;

    for (vi, p) in paths.split(',').enumerate() {
        let kv = squeezefs::meta_backend::kv::backend::KvMetaBackend::open(std::path::Path::new(p))
            .await
            .unwrap_or_else(|e| panic!("open {p}: {e:?}"));
        use squeezefs::meta_backend::kv::record::{decode_inode_key, inode_key, InodeValue};
        let inodes = kv.flat_trees()[0].clone();
        let mut cursor: Vec<u8> = inode_key(1).to_vec();
        let end = inode_key(u64::MAX - 1);
        loop {
            let page = inodes.range(&cursor, &end, 512).await.expect("range");
            let Some((last_key, _)) = page.last() else {
                break;
            };
            cursor = squeezefs::meta_backend::kv::node::key_successor(last_key);
            for (k, v) in &page {
                let Ok(ino) = decode_inode_key(k) else {
                    continue;
                };
                let Ok(val) = InodeValue::decode(v) else {
                    continue;
                };
                if val.nlink == 0 {
                    continue;
                }
                if let Ok(Some(bytes)) = kv.getxattr(ino, "layout").await {
                    total_layouts += 1;
                    let layout: Option<squeezefs::routing::LayoutMetadata> =
                        if bytes.starts_with(b"{") {
                            serde_json::from_slice(&bytes).ok()
                        } else {
                            bincode::deserialize(&bytes).ok()
                        };
                    let Some(layout) = layout else { continue };
                    if layout.file_type != "striped" {
                        continue;
                    }
                    striped += 1;
                    if let Some(id) = &layout.block_map_id {
                        if id.starts_with("indirect:") {
                            *per_prefix.entry("INDIRECT-MAP".into()).or_default() += 1;
                        }
                    }
                    if let Some(bm) = &layout.block_map {
                        for (b, key) in bm {
                            let (prefix, off) = match key.find("://") {
                                Some(pos) => (
                                    key[..pos].to_string(),
                                    key[pos + 3..]
                                        .split(':')
                                        .next()
                                        .unwrap_or("")
                                        .parse::<u64>()
                                        .unwrap_or(u64::MAX),
                                ),
                                None => (
                                    "BARE".to_string(),
                                    key.split(':')
                                        .next()
                                        .unwrap_or("")
                                        .parse()
                                        .unwrap_or(u64::MAX),
                                ),
                            };
                            *per_prefix.entry(prefix.clone()).or_default() += 1;
                            owners.entry((prefix, off)).or_default().push((vi, ino, *b));
                        }
                    }
                }
            }
        }
    }
    println!("layouts={total_layouts} striped={striped}");
    let mut forms: Vec<_> = per_prefix.into_iter().collect();
    forms.sort();
    for (form, n) in &forms {
        println!("key-form {form}: {n}");
    }
    // Double-references: one (be,offset) named by >1 (vol,ino,block) is a
    // cross-file alias — corruption evidence (refcount clones aside, bench
    // files are never cloned).
    let mut dupes = 0;
    for ((be, off), refs) in &owners {
        if refs.len() > 1 {
            dupes += 1;
            if dupes <= 10 {
                println!("DUPE {be}://{off} referenced by {refs:?}");
            }
        }
    }
    println!("dupe-referenced offsets: {dupes}");
}
