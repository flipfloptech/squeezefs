//! Finding A (2026-07-13 beta release gate) — node-seq re-mint across
//! clean remounts ⇒ recycled-extent frame admission.
//!
//! **Mechanism** (root-caused from the gate artifacts + the seq-domain
//! probe): node incarnation seqs are minted from a per-volume counter
//! that is never persisted. A mount reseeds it from
//! `max(ledger checkpoint ordinal, the three ROOT node seqs, the
//! replay-window interior child pointers)`. Record (LWW) seqs are
//! journal-domain and safe — but NODE seqs are not: any non-root node
//! minted after the last root SMO (a leaf split/compaction under an
//! interior root appends the parent pointer without re-minting the
//! root) sits ABOVE every mount floor once a clean shutdown empties the
//! replay window. The next session re-mints those seq values; a freed
//! extent still holding appended bset frames stamped with them can be
//! reallocated to a new node with an EQUAL seq — and the §4.5
//! `node_seq_at_write == node_seq` chain check then admits the previous
//! incarnation's checksummed frames as the new node's own records
//! (dentry-under-interior "interior value must be 16 bytes, got 75" /
//! foreign-value "dentry value: N trailing byte(s)" — the release-gate
//! scratch signature).
//!
//! Contracts pinned (red-first on the un-fixed tree):
//!  1. `node_seq_mints_stay_above_every_persisted_stamp_across_remount`
//!     — the mint invariant itself, observed offline: no node seq
//!     minted in session 2 may repeat/undershoot any node-seq stamp
//!     persisted by session 1.
//!  2. `future_stamped_residue_frame_is_buried_never_admitted` — the
//!     node-layer burial contract for the higher-stamp direction:
//!     residue stamped above the live incarnation (the shape re-minting
//!     produced, and the shape dead-generation reformat residue takes on
//!     a coin flip) is never admitted and never fails the load.

use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options, ROOT_INO};
use squeezefs::meta_backend::kv::checkpoint::read_newest_ledger;
use squeezefs::meta_backend::kv::node::{
    append_bset, load_node, AppendDest, NodeLayout, NodeWriteParams,
};
use squeezefs::meta_backend::kv::record::{DentryValue, Record, RecordKind};
use squeezefs::meta_backend::kv::superblock::{classify_volume, VolumeFormat};
use squeezefs::meta_backend::kv::tree::decode_interior_value;
use squeezefs::meta_backend::{open_volume_for_mount, Metadata};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tempfile::NamedTempFile;

const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;

async fn fresh_volume() -> (Arc<KvMetaBackend>, NamedTempFile) {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(VOL_LEN).unwrap();
    format_v3(
        file.path(),
        VOL_LEN,
        &FormatV3Options {
            node_size: NODE_SIZE,
            journal_len_override: Some(RING_LEN),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .unwrap();
    let backend = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .expect("open_volume_for_mount");
    (backend, file)
}

/// Offline walk of every live node reachable from the mounted ledger's
/// roots: fold each interior node's newest-per-key pointer records and
/// descend. Returns `(addr → header node_seq)` for every live node.
async fn walk_live_node_seqs(path: &Path) -> HashMap<u64, u64> {
    let sb = match classify_volume(path).await.expect("classify") {
        VolumeFormat::V3(sb) => sb,
        other => panic!("not a v3 volume: {other:?}"),
    };
    let ledger = read_newest_ledger(path, sb.root_ledger.start)
        .await
        .expect("ledger read")
        .expect("a ledger record exists");
    let layout = NodeLayout::new(sb.node_size as usize).expect("layout");
    let mut out: HashMap<u64, u64> = HashMap::new();
    let mut stack: Vec<u64> = ledger.tree_roots.iter().map(|r| r.node_addr).collect();
    while let Some(addr) = stack.pop() {
        if out.contains_key(&addr) {
            continue;
        }
        let node = load_node(path, &layout, addr, ledger.journal_tail_seq)
            .await
            .unwrap_or_else(|e| panic!("load node {addr:#x}: {e}"));
        out.insert(addr, node.header().node_seq);
        if node.header().level > 0 {
            // Newest-per-key fold over all bsets (positional append
            // order per bset; seq is the fold key).
            let mut newest: HashMap<Vec<u8>, (u64, RecordKind, Vec<u8>)> = HashMap::new();
            for bi in 0..node.bset_count() {
                let view = node.bset(bi).expect("bset parses");
                for rec in view.iter() {
                    let e = newest.entry(rec.key.to_vec()).or_insert((
                        rec.seq,
                        rec.kind,
                        rec.value.to_vec(),
                    ));
                    if rec.seq >= e.0 {
                        *e = (rec.seq, rec.kind, rec.value.to_vec());
                    }
                }
            }
            for (_k, (_seq, kind, value)) in newest {
                if kind == RecordKind::Put {
                    let (child_addr, _child_seq) =
                        decode_interior_value(&value).expect("interior value decodes");
                    stack.push(child_addr);
                }
            }
        }
    }
    out
}

/// The newest ledger's root-seq maximum — the un-fixed tree's mount
/// floor for the node-seq mint counter after a clean shutdown.
async fn ledger_root_seq_max(path: &Path) -> u64 {
    let sb = match classify_volume(path).await.expect("classify") {
        VolumeFormat::V3(sb) => sb,
        other => panic!("not v3: {other:?}"),
    };
    let ledger = read_newest_ledger(path, sb.root_ledger.start)
        .await
        .expect("ledger")
        .expect("some ledger");
    ledger
        .tree_roots
        .iter()
        .map(|r| r.node_seq)
        .max()
        .expect("roots")
}

/// Drive the dentry tree through enough create/unlink churn to force
/// non-root structure activity (compactions / splits under the root).
async fn churn(be: &KvMetaBackend, dir_ino: u64, round: usize, files: usize) {
    for i in 0..files {
        let name = format!("r{round}-longish-name-padding-{i:05}");
        be.create(dir_ino, &name, libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap_or_else(|e| panic!("create {name}: {e}"));
    }
    for i in 0..files {
        let name = format!("r{round}-longish-name-padding-{i:05}");
        be.unlink(dir_ino, &name)
            .await
            .unwrap_or_else(|e| panic!("unlink {name}: {e}"));
    }
}

/// Contract 1 — the mint invariant: after a clean shutdown, a remounted
/// volume must never mint a node seq at or below ANY node-seq stamp the
/// previous session persisted (live nodes observed offline are a lower
/// bound on the stamps; freed-extent residue carries the same domain).
/// On the un-fixed tree the mount floor is the ROOT seqs only, while
/// session 1 deliberately ends with non-root mints above them — session
/// 2's fresh mints then repeat the stamped range: RED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_seq_mints_stay_above_every_persisted_stamp_across_remount() {
    let (backend, file) = fresh_volume().await;

    // Session 1: churn hard enough that leaf-level SMOs mint node seqs
    // AFTER the last root mint (tombstone-desert compactions append the
    // parent pointer without re-minting the root).
    let dir = backend
        .create(ROOT_INO, "churn", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    for round in 0..6 {
        churn(&backend, dir.ino, round, 400).await;
    }
    backend.shutdown().await.expect("clean shutdown 1");
    drop(backend);

    // Precondition shaping: session 1 must END with non-root mints well
    // above the root seqs (the un-fixed mount floor), so session 2's
    // handful of fresh mints land deterministically inside the stamped
    // domain. Root SMOs re-mint the root at the top of the counter, so
    // whether a run ends gap-wide is cadence-dependent — churn in
    // bounded extra rounds until the gap is comfortably wider than one
    // session-2 round can mint. Post-fix this loop is irrelevant: the
    // assertion below must hold for ANY gap.
    let (s1, s1_max) = {
        let mut extra = 0usize;
        loop {
            let s1 = walk_live_node_seqs(file.path()).await;
            let s1_max = *s1.values().max().expect("session 1 has live nodes");
            let root_max = ledger_root_seq_max(file.path()).await;
            if s1_max >= root_max + 24 {
                break (s1, s1_max);
            }
            if extra >= 12 {
                // Cadence never produced a wide gap. On the un-fixed tree
                // this weakens the red (session-2 mints may cross the
                // ceiling); on the FIXED tree the assertion below is
                // gap-independent (the watermark floors the reseed at
                // s1_max regardless), so proceed with what we have.
                break (s1, s1_max);
            }
            let be = KvMetaBackend::open(file.path()).await.expect("re-open for shaping");
            churn(&be, dir.ino, 50 + extra, 400).await;
            be.shutdown().await.expect("shaping shutdown");
            drop(be);
            extra += 1;
        }
    };

    // Sessions 2..=5: the fstests cadence — every cycle remounts (empty
    // replay window = the floor collapse under test), forces fresh SMO
    // mints, cleanly shuts down, and is observed offline against the
    // RUNNING ceiling of every stamp any prior session persisted. A
    // collapsed-floor tree escapes one cycle's detection only when a
    // late root SMO happens to re-floor it near the ceiling; across
    // four cycles an all-green run was ~1-in-5 on the un-fixed tree
    // (observed), so any regression flips the vast majority of rolls
    // red. The FIXED tree is exact: the watermark floors every cycle at
    // or above the ceiling, so green is unconditional, not statistical.
    let mut prev = s1;
    let mut ceiling = s1_max;
    let mut violations: Vec<String> = Vec::new();
    for cycle in 0..4 {
        let re = KvMetaBackend::open(file.path()).await.expect("remount");
        assert_eq!(
            re.replay_stats().entries,
            0,
            "precondition: clean shutdown ⇒ empty replay window (cycle {cycle})"
        );
        let dir2 = re
            .create(
                ROOT_INO,
                &format!("churn2-{cycle}"),
                libc::S_IFDIR | 0o755,
                0,
                0,
            )
            .await
            .unwrap();
        // ONE round per cycle: enough churn to force at least one leaf
        // compaction (a fresh mint), few enough that survivors sit just
        // above the collapsed floor on the un-fixed tree.
        churn(&re, dir2.ino, 100 + cycle, 400).await;
        re.shutdown().await.expect("cycle clean shutdown");
        drop(re);

        let cur = walk_live_node_seqs(file.path()).await;
        for (addr, seq) in &cur {
            let is_new_incarnation = prev.get(addr) != Some(seq);
            if is_new_incarnation && *seq <= ceiling {
                violations.push(format!(
                    "cycle {cycle}: node {addr:#x} minted seq {seq} ≤ stamp ceiling {ceiling}"
                ));
            }
        }
        ceiling = ceiling.max(*cur.values().max().expect("live nodes"));
        prev = cur;
    }
    assert!(
        violations.is_empty(),
        "node-seq mints repeated a previous session's stamped domain \
         (recycled-extent frames with these stamps become admissible — Finding A):\n  {}",
        violations.join("\n  ")
    );
}

/// Contract 2 — residue is NEVER admitted, in either stamp direction.
/// Within one generation the watermark makes residue strictly older;
/// across a quick reformat, dead-generation residue carries a foreign
/// uuid-derived stamp that is HIGHER on a coin flip — and must stay
/// silently buried (a loud higher-stamp tripwire was evaluated and
/// rejected: it would fail legitimate post-reformat mounts; see
/// `FrameProbe::StaleIncarnation`). This pins both properties for the
/// higher-stamp direction: the walk neither admits the foreign frame
/// nor fails the load.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn future_stamped_residue_frame_is_buried_never_admitted() {
    let file = NamedTempFile::new().expect("temp file");
    file.as_file().set_len(2 * 1024 * 1024).unwrap();
    let layout = NodeLayout::new(NODE_SIZE).expect("layout");
    let path = file.path();

    // A legitimate leaf at extent 0, seq 1000, with one base record
    // (a well-formed dentry value — the write-side audit rejects
    // malformed encodes by design).
    let base = vec![Record {
        key: vec![1u8; 16],
        seq: 7,
        kind: RecordKind::Put,
        value: DentryValue {
            child_ino: 42,
            file_type: 8,
            name: b"base-entry".to_vec(),
        }
        .encode()
        .expect("dentry encodes"),
    }];
    let written = squeezefs::meta_backend::kv::node::write_node(
        path,
        &layout,
        &NodeWriteParams {
            node_addr: 0,
            node_seq: 1000,
            tree_id: 2,
            level: 0,
            min_key: b"",
            max_key: &[0xFF; 32],
        },
        &base,
        7,
    )
    .await
    .expect("write_node");

    // Forge residue BEYOND the live tail stamped by a FUTURE incarnation
    // (seq 2000 > 1000) — the state Finding A's re-mint hole produces
    // when an extent is recycled by a LOWER-seq node.
    let residue = vec![Record {
        key: vec![2u8; 16],
        seq: 9,
        kind: RecordKind::Put,
        value: DentryValue {
            child_ino: 43,
            file_type: 8,
            name: b"residue-entry".to_vec(),
        }
        .encode()
        .expect("dentry encodes"),
    }];
    append_bset(
        path,
        &layout,
        &AppendDest {
            node_addr: 0,
            node_seq: 2000,
            tail_offset: written.bytes_written,
        },
        &residue,
        9,
    )
    .await
    .expect("forge future-stamped residue frame");

    let node = load_node(path, &layout, 0, u64::MAX)
        .await
        .expect("higher-stamped residue must not fail the load (reformat burial)");
    assert_eq!(
        node.bset_count(),
        1,
        "higher-stamped residue must never be admitted into the population"
    );
    assert_eq!(
        node.tail_offset(),
        written.bytes_written,
        "the log tail must stop at the live incarnation's last frame"
    );
    let folded = node
        .lookup(&[2u8; 16])
        .expect("fold over the loaded population");
    assert!(
        folded.live_value().is_none(),
        "the residue record's key must not resolve"
    );
}
