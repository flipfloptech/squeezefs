//! FIND-VS-A fold-completeness fixture: replays a REAL storm split's fold
//! inputs (predecessor bset bytes + frozen-extra records, extracted from a
//! post-kill meta image) through the production `compact()` and asserts no
//! live key is dropped. The 2026-07-16 forensics found split successors
//! missing 14 predecessor keys (acked creates → ENOENT after remount).
use squeezefs::meta_backend::kv::bset::{build_bset, compact, BsetView};
use squeezefs::meta_backend::kv::record::{Record, RecordKind};

fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{path}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture present")
}

#[test]
fn real_storm_split_fold_preserves_every_live_key() {
    let pred_bytes = fixture("findvsa_pred_bset.bin");
    let pred = BsetView::parse(&pred_bytes).expect("pred bset parses");

    let mut extra: Vec<Record> = Vec::new();
    for line in String::from_utf8(fixture("findvsa_extra_records.jsonl"))
        .expect("utf8")
        .lines()
    {
        let v: serde_json::Value = serde_json::from_str(line).expect("jsonl");
        let key = hex_decode(v[0].as_str().unwrap());
        let kind = match v[1].as_u64().unwrap() {
            1 => RecordKind::Put,
            2 => RecordKind::Delta,
            3 => RecordKind::Delete,
            k => panic!("kind {k}"),
        };
        let seq = v[2].as_u64().unwrap();
        let val = hex_decode(v[3].as_str().unwrap());
        extra.push(Record {
            key,
            seq,
            kind,
            value: val,
        });
    }
    let extra_horizon = extra.iter().map(|r| r.seq).max().unwrap_or(0);
    let extra_image = build_bset(&extra, extra_horizon).expect("extra builds");
    let extra_view = BsetView::parse(&extra_image).expect("extra parses");

    // The union of live input keys (no tombstones in this capture).
    let mut want: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    for r in pred.iter() {
        want.insert(r.key.to_vec());
    }
    for r in &extra {
        want.insert(r.key.clone());
    }

    // durable_tail at the real split (~ the ledger tail of that cycle):
    // higher than every input seq — the harshest legal elision setting.
    let durable_tail = 76_036_303u64;
    let views = vec![extra_view, pred];
    let folded = compact(&views, durable_tail).expect("compact folds");
    let got: std::collections::BTreeSet<Vec<u8>> = folded.iter().map(|r| r.key.clone()).collect();

    let lost: Vec<String> = want
        .difference(&got)
        .map(|k| k.iter().map(|b| format!("{b:02x}")).collect())
        .collect();
    assert!(
        lost.is_empty(),
        "compact() dropped {} live key(s): {:?}",
        lost.len(),
        &lost[..lost.len().min(20)]
    );
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// The 2026-07-16 re-captured loss round (docs/design-smo-replay-currency.md
/// §6 PR 1 first act; regeneration: the FIND-VS-A harness — removed from
/// the tree, git history at `c615e3a`: `.agents/findvsa/recapture.sh`, raw
/// images out-of-repo, expectations in
/// `.agents/findvsa/capture-2026-07-16-expectations.txt`): 311 acked
/// creates lost across a CLEAN replay (`dropped_torn == [0,0,0,0]`). The
/// fixture is the kvparse.py-derived window subset — every lost name's
/// acked Put with its replay-window position, plus the in-window
/// higher-seq interior flips covering their keys. This test re-asserts
/// the two mechanism pins from the real bytes:
///
/// 1. **Every lost record was IN the replay window** (`seq ≥ tail` of its
///    volume's mounted ledger record) — the loss is not a durability
///    hole; replay saw the records and the walk routed around them.
/// 2. **The sub-mechanism (i) stranding signature** (design §1): the
///    majority carry an in-window interior flip with `flip.seq > put.seq`
///    and `flip.key ≥ put.key` — the record replays via the pre-flip
///    route, then the higher-seq flip abandons that lineage. (The
///    remainder are the flip-less faces: root-swap windows and reuse —
///    Option A's rows, not C′'s.)
#[test]
fn recaptured_window_pins_the_stranding_signature() {
    let raw = String::from_utf8(fixture("findvsa2_stranding_window.jsonl")).expect("utf8");
    let mut lines = raw.lines();
    let hdr: serde_json::Value =
        serde_json::from_str(lines.next().expect("header line")).expect("header json");
    assert_eq!(hdr["row"], "header");
    let loss_total = hdr["loss_total"].as_u64().expect("loss_total");
    let in_window = hdr["in_window"].as_u64().expect("in_window");
    let stranding_sig = hdr["stranding_sig"].as_u64().expect("stranding_sig");
    assert!(loss_total >= 1, "a loss round was captured");
    assert_eq!(
        in_window, loss_total,
        "every acked-lost record must be IN the replay window (clean-replay loss)"
    );

    #[allow(clippy::type_complexity)]
    let mut lost: Vec<(u64, u64, Vec<u8>, u64, Option<u64>, Vec<u8>, u64)> = Vec::new();
    let mut flips: Vec<(u64, u64, u64, Vec<u8>)> = Vec::new(); // (img, tree, seq, key)
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("row json");
        match v["row"].as_str().expect("row kind") {
            "lost_put" => {
                let d = &v["dentry"];
                let i = &v["inode"];
                lost.push((
                    d["img"].as_u64().unwrap(),
                    d["seq"].as_u64().unwrap(),
                    hex_decode(d["key"].as_str().unwrap()),
                    d["tail"].as_u64().unwrap(),
                    i["seq"].as_u64(),
                    hex_decode(i["key"].as_str().unwrap()),
                    i["img"].as_u64().unwrap(),
                ));
            }
            "flip" => flips.push((
                v["img"].as_u64().unwrap(),
                v["tree"].as_u64().unwrap(),
                v["seq"].as_u64().unwrap(),
                hex_decode(v["key"].as_str().unwrap()),
            )),
            other => panic!("unknown fixture row {other}"),
        }
    }
    assert_eq!(lost.len() as u64, loss_total, "one row per lost name");

    const TREE_INODES_ID: u64 = 1;
    const TREE_DENTRIES_ID: u64 = 2;
    let mut recomputed = 0u64;
    for (dimg, dseq, dkey, dtail, iseq, ikey, iimg) in &lost {
        // Pin 1: in-window (key lengths sanity-check the kv key domains).
        assert!(dseq >= dtail, "lost dentry Put below its mounted tail");
        assert_eq!(dkey.len(), 16, "dentry key is (parent, hash54, coll_seq)");
        assert_eq!(ikey.len(), 8, "inode key is a BE ino");
        let d_cov = flips.iter().any(|(img, tree, fseq, fkey)| {
            img == dimg && *tree == TREE_DENTRIES_ID && fseq > dseq && fkey[..] >= dkey[..]
        });
        let i_cov = iseq.is_some_and(|is| {
            flips.iter().any(|(img, tree, fseq, fkey)| {
                img == iimg && *tree == TREE_INODES_ID && *fseq > is && fkey[..] >= ikey[..]
            })
        });
        if d_cov || i_cov {
            recomputed += 1;
        }
    }
    // Pin 2: the signature recomputes from the raw rows and is the
    // majority shape of the capture.
    assert_eq!(
        recomputed, stranding_sig,
        "stranding signature must recompute from the fixture rows"
    );
    assert!(
        stranding_sig * 2 >= loss_total,
        "the (i) stranding signature carries the majority of the captured loss \
         ({stranding_sig}/{loss_total})"
    );
}
