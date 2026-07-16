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
