//! Contracts for the redesigned `squeezefs bench` (simplified-elbencho
//! model, 2026-07-11): explicit phases over a persistent reusable dataset,
//! one shape vocabulary, user-settable I/O block size.
//!
//! The engine (`squeezefs::bench`) is FUSE-independent by design — it
//! benchmarks any directory — so these tests drive it against scratch
//! directories. CLI-surface contracts (old flags deleted, new surface
//! accepted) drive the real binary via `CARGO_BIN_EXE_squeezefs`.

use rstest::rstest;
use squeezefs::bench::{
    bench_file_path, block_count, block_order, block_seed, dataset_root, fill_block, parse_size,
    run_cli, run_phases, select_phases, validate_dataset, validate_shape, BenchError, Phase,
    PhaseResult, Shape,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Scratch under ~/tmp (repo discipline: scratch lives in ~/tmp).
fn scratch(tag: &str) -> PathBuf {
    let home = std::env::var("HOME").expect("HOME set");
    let base = PathBuf::from(home)
        .join("tmp")
        .join(format!("sqfs_bench_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create scratch dir");
    base
}

fn cleanup(base: &Path) {
    let _ = std::fs::remove_dir_all(base);
}

fn shape(threads: usize, files: usize, size: u64, block: u64) -> Shape {
    Shape {
        threads,
        files,
        size,
        block,
        rand: false,
        direct: false,
    }
}

/// Regenerate the expected deterministic pattern for a whole file and
/// compare against the on-disk bytes, block by block (partial tail
/// included). Any never-written block would read back as zeros/holes and
/// fail here — this is the full-coverage proof for `--rand`.
fn assert_file_matches_pattern(mount: &Path, sh: &Shape, tid: usize, fid: usize) {
    let path = bench_file_path(mount, tid, fid);
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("read back {path:?}: {e}"));
    assert_eq!(
        data.len() as u64,
        sh.size,
        "file {path:?} must be exactly the shaped size"
    );
    let nblocks = block_count(sh);
    for b in 0..nblocks {
        let off = (b * sh.block) as usize;
        let len = std::cmp::min(sh.block, sh.size - b * sh.block) as usize;
        let mut expected = vec![0u8; len];
        fill_block(&mut expected, tid, fid, b);
        assert_eq!(
            &data[off..off + len],
            &expected[..],
            "block {b} of {path:?} must carry the deterministic (tid,fid,block) pattern \
             (a mismatch means the block was skipped or double-written)"
        );
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_squeezefs")
}

// ---------------------------------------------------------------------------
// parse_size: human units 4k/128k/4m/10g + plain bytes, garbage rejected
// ---------------------------------------------------------------------------

#[rstest]
#[case("4k", 4096)]
#[case("128k", 131_072)]
#[case("1m", 1_048_576)]
#[case("4m", 4 * 1024 * 1024)]
#[case("1g", 1024 * 1024 * 1024)]
#[case("10g", 10u64 * 1024 * 1024 * 1024)]
#[case("4K", 4096)] // case-insensitive
#[case("1M", 1_048_576)]
#[case("2G", 2u64 * 1024 * 1024 * 1024)]
#[case("1048576", 1_048_576)] // plain bytes
#[case("512", 512)]
#[case(" 1m ", 1_048_576)] // surrounding whitespace tolerated
fn test_parse_size_accepts_human_units(#[case] input: &str, #[case] expected: u64) {
    assert_eq!(
        parse_size(input).unwrap_or_else(|e| panic!("{input:?} must parse: {e}")),
        expected,
        "parse_size({input:?})"
    );
}

#[rstest]
#[case("")]
#[case("k")]
#[case("4x")]
#[case("4kk")]
#[case("-1")]
#[case("4.5m")]
#[case("0")] // zero-byte sizes are useless and rejected loudly
#[case("0k")]
#[case("m4")]
#[case("99999999999999999999g")] // overflow
fn test_parse_size_rejects_garbage(#[case] input: &str) {
    assert!(
        parse_size(input).is_err(),
        "parse_size({input:?}) must be rejected"
    );
}

// ---------------------------------------------------------------------------
// phase selection: empty => write+read; fixed order write,read,stat,del
// ---------------------------------------------------------------------------

#[test]
fn test_phase_selection_default_is_write_read() {
    assert_eq!(
        select_phases(false, false, false, false),
        vec![Phase::Write, Phase::Read],
        "no phase flags must default to write+read"
    );
}

#[rstest]
#[case(true, false, false, false, vec![Phase::Write])]
#[case(false, true, false, false, vec![Phase::Read])]
#[case(false, false, true, false, vec![Phase::Stat])]
#[case(false, false, false, true, vec![Phase::Del])]
#[case(true, true, false, false, vec![Phase::Write, Phase::Read])]
#[case(true, false, false, true, vec![Phase::Write, Phase::Del])]
#[case(false, true, true, false, vec![Phase::Read, Phase::Stat])]
#[case(true, true, true, true, vec![Phase::Write, Phase::Read, Phase::Stat, Phase::Del])]
fn test_phase_selection_fixed_order(
    #[case] w: bool,
    #[case] r: bool,
    #[case] s: bool,
    #[case] d: bool,
    #[case] expected: Vec<Phase>,
) {
    assert_eq!(
        select_phases(w, r, s, d),
        expected,
        "phase order is fixed (write, read, stat, del) regardless of flag order"
    );
}

// ---------------------------------------------------------------------------
// shape validation: zero counts + O_DIRECT alignment contract
// ---------------------------------------------------------------------------

#[test]
fn test_shape_rejects_zero_threads_and_files() {
    let mut sh = shape(0, 1, 4096, 4096);
    assert!(matches!(validate_shape(&sh), Err(BenchError::Shape(_))));
    sh = shape(1, 0, 4096, 4096);
    assert!(matches!(validate_shape(&sh), Err(BenchError::Shape(_))));
}

#[test]
fn test_shape_rejects_zero_size_and_block() {
    let mut sh = shape(1, 1, 0, 4096);
    assert!(matches!(validate_shape(&sh), Err(BenchError::Shape(_))));
    sh = shape(1, 1, 4096, 0);
    assert!(matches!(validate_shape(&sh), Err(BenchError::Shape(_))));
}

#[test]
fn test_direct_requires_block_multiple_of_4096() {
    // -b 10k = 10240 bytes: NOT a multiple of 4096 => loud refusal.
    // (size is a multiple of block so only the 4096 rule can fire.)
    let mut sh = shape(1, 1, 100 * 1024, 10 * 1024);
    sh.direct = true;
    let err = validate_shape(&sh).expect_err("--direct with -b 10k must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("4096"),
        "error must explain the 4096-byte alignment requirement, got: {msg}"
    );
    // Same block size without --direct is fine (partial tail allowed).
    sh.direct = false;
    validate_shape(&sh).expect("non-direct unaligned block is allowed");
}

#[test]
fn test_direct_100k_block_rejected_via_size_multiple_rule() {
    // The user-facing "-b 100k" refusal: 100k = 102400 bytes IS 4096-aligned
    // (25 * 4096), so the rejection comes from -s % -b != 0 (O_DIRECT EOF
    // tail trap) for any size that is not a 100k multiple — e.g. -s 1g.
    let mut sh = shape(1, 1, 1024 * 1024 * 1024, 100 * 1024);
    sh.direct = true;
    let err = validate_shape(&sh).expect_err("--direct -b 100k -s 1g must be refused");
    assert!(
        err.to_string().contains("multiple"),
        "error must explain the size-multiple-of-block requirement, got: {err}"
    );
    // --direct -b 128k -s 1g is the accepted counterpart.
    sh.block = 128 * 1024;
    validate_shape(&sh).expect("--direct -b 128k -s 1g must validate");
}

#[test]
fn test_direct_requires_size_multiple_of_block() {
    // O_DIRECT EOF tail writes are a trap: -s must be a multiple of -b.
    let mut sh = shape(1, 1, 1024 * 1024 + 4096, 128 * 1024);
    sh.direct = true;
    let err = validate_shape(&sh).expect_err("--direct with size % block != 0 must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("multiple"),
        "error must explain the size-multiple-of-block requirement, got: {msg}"
    );
    // Aligned combination is accepted: --direct -b 128k -s 1m.
    sh.size = 1024 * 1024;
    validate_shape(&sh).expect("--direct -b 128k -s 1m must validate");
}

// ---------------------------------------------------------------------------
// dataset layout + validation: loud found-vs-expected, nothing created
// ---------------------------------------------------------------------------

#[test]
fn test_dataset_layout_paths() {
    let mount = Path::new("/mnt/x");
    assert_eq!(
        dataset_root(mount),
        Path::new("/mnt/x/squeezefs-bench").to_path_buf()
    );
    assert_eq!(
        bench_file_path(mount, 2, 7),
        Path::new("/mnt/x/squeezefs-bench/t2/f7.bin").to_path_buf()
    );
}

#[test]
fn test_validate_dataset_missing_is_loud_and_creates_nothing() {
    let base = scratch("val_missing");
    let sh = shape(2, 3, 64 * 1024, 16 * 1024);
    let err = validate_dataset(&base, &sh).expect_err("missing dataset must fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("6"),
        "error must state the expected file count (6), got: {msg}"
    );
    assert!(
        msg.contains("-w"),
        "error must tell the user to run the write phase (-w), got: {msg}"
    );
    assert!(
        !dataset_root(&base).exists(),
        "validation must never create the dataset directory"
    );
    cleanup(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_validate_dataset_wrong_size_is_loud() {
    let base = scratch("val_size");
    let sh = shape(1, 2, 32 * 1024, 8 * 1024);
    run_phases(&base, &[Phase::Write], &sh)
        .await
        .expect("write phase");
    // Corrupt one file's size.
    let victim = bench_file_path(&base, 0, 1);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&victim)
        .expect("open victim");
    f.set_len(12345).expect("truncate victim");
    drop(f);

    let err = validate_dataset(&base, &sh).expect_err("size mismatch must fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("12345") && msg.contains("32768"),
        "error must report found-vs-expected size, got: {msg}"
    );
    assert!(msg.contains("-w"), "error must point at -w, got: {msg}");
    cleanup(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_validate_dataset_extra_files_is_loud() {
    let base = scratch("val_extra");
    let sh = shape(2, 3, 16 * 1024, 16 * 1024);
    run_phases(&base, &[Phase::Write], &sh)
        .await
        .expect("write phase");
    std::fs::write(dataset_root(&base).join("t0").join("f99.bin"), b"stray")
        .expect("plant stray file");
    let err = validate_dataset(&base, &sh).expect_err("stray files must fail validation");
    let msg = err.to_string();
    assert!(
        msg.contains("7") && msg.contains("6"),
        "error must report found-vs-expected counts (7 vs 6), got: {msg}"
    );
    cleanup(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_validate_dataset_ok_after_write() {
    let base = scratch("val_ok");
    let sh = shape(2, 3, 64 * 1024, 16 * 1024);
    run_phases(&base, &[Phase::Write], &sh)
        .await
        .expect("write phase");
    validate_dataset(&base, &sh).expect("freshly written dataset must validate");
    cleanup(&base);
}

// ---------------------------------------------------------------------------
// block order: seq identity; --rand shuffled full-coverage permutation
// ---------------------------------------------------------------------------

#[test]
fn test_block_order_sequential_is_identity() {
    assert_eq!(block_order(false, 5), vec![0, 1, 2, 3, 4]);
}

#[test]
fn test_block_order_rand_is_full_coverage_permutation() {
    for n in [1u64, 2, 17, 256] {
        let mut order = block_order(true, n);
        assert_eq!(order.len() as u64, n, "rand order must have {n} entries");
        order.sort_unstable();
        let expected: Vec<u64> = (0..n).collect();
        assert_eq!(
            order, expected,
            "rand order must visit every block exactly once (n={n})"
        );
    }
}

#[test]
fn test_block_count_partial_tail() {
    // 20k / 16k => 2 ops (one full + one 4k tail).
    assert_eq!(block_count(&shape(1, 1, 20 * 1024, 16 * 1024)), 2);
    assert_eq!(block_count(&shape(1, 1, 64 * 1024, 16 * 1024)), 4);
    assert_eq!(block_count(&shape(1, 1, 4 * 1024, 16 * 1024)), 1);
}

// ---------------------------------------------------------------------------
// write pattern: deterministic per (tid,fid,block), non-zero
// ---------------------------------------------------------------------------

#[test]
fn test_fill_block_deterministic_and_nonzero() {
    let mut a = vec![0u8; 4096];
    let mut b = vec![0u8; 4096];
    fill_block(&mut a, 1, 2, 3);
    fill_block(&mut b, 1, 2, 3);
    assert_eq!(a, b, "same (tid,fid,block) must produce identical bytes");
    assert!(
        a.iter().any(|&x| x != 0),
        "pattern must not be all-zeros (compression would fake numbers)"
    );

    let mut c = vec![0u8; 4096];
    fill_block(&mut c, 1, 2, 4);
    assert_ne!(a, c, "different blocks must produce different bytes");
    assert_ne!(block_seed(0, 0, 0), block_seed(0, 0, 1));
    assert_ne!(block_seed(0, 0, 0), block_seed(0, 1, 0));
    assert_ne!(block_seed(0, 0, 0), block_seed(1, 0, 0));
}

// ---------------------------------------------------------------------------
// end-to-end on a scratch dir: -t 2 -n 3 -s 64k -b 16k, all four phases
// ---------------------------------------------------------------------------

fn assert_sane_result(res: &PhaseResult, expect_bytes: u64, expect_ops: u64) {
    assert_eq!(res.ops, expect_ops, "{:?}: ops", res.phase);
    assert_eq!(res.bytes, expect_bytes, "{:?}: bytes", res.phase);
    assert!(
        res.elapsed > Duration::ZERO,
        "{:?}: elapsed must be nonzero",
        res.phase
    );
    assert!(res.iops() > 0.0, "{:?}: IOPS must be nonzero", res.phase);
    if expect_bytes > 0 {
        assert!(
            res.throughput_mib_s() > 0.0,
            "{:?}: throughput must be nonzero",
            res.phase
        );
    }
    assert!(
        res.lat_min <= res.lat_avg && res.lat_avg <= res.lat_max,
        "{:?}: min<=avg<=max violated ({:?} {:?} {:?})",
        res.phase,
        res.lat_min,
        res.lat_avg,
        res.lat_max
    );
    assert!(
        res.lat_min <= res.lat_p99 && res.lat_p99 <= res.lat_max,
        "{:?}: min<=p99<=max violated ({:?} {:?} {:?})",
        res.phase,
        res.lat_min,
        res.lat_p99,
        res.lat_max
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_end_to_end_all_four_phases() {
    let base = scratch("e2e");
    let sh = shape(2, 3, 64 * 1024, 16 * 1024);
    let data_bytes = 2 * 3 * 64 * 1024u64;
    let io_ops = 2 * 3 * 4u64; // 4 blocks per file
    let meta_ops = 2 * 3u64; // one op per file

    // Write alone: dataset must exist with exact sizes + pattern.
    let report = run_phases(&base, &[Phase::Write], &sh)
        .await
        .expect("write phase");
    assert_eq!(report.phases.len(), 1);
    assert_eq!(report.phases[0].phase, Phase::Write);
    assert_sane_result(&report.phases[0], data_bytes, io_ops);
    for tid in 0..2 {
        for fid in 0..3 {
            let p = bench_file_path(&base, tid, fid);
            let md = std::fs::metadata(&p).unwrap_or_else(|e| panic!("{p:?} must exist: {e}"));
            assert_eq!(md.len(), 64 * 1024, "{p:?} size");
            assert_file_matches_pattern(&base, &sh, tid, fid);
        }
    }

    // Read reuses the persistent dataset (separate invocation == persistence).
    let report = run_phases(&base, &[Phase::Read], &sh)
        .await
        .expect("read phase");
    assert_eq!(report.phases[0].phase, Phase::Read);
    assert_sane_result(&report.phases[0], data_bytes, io_ops);

    // Full ordered set in one call.
    let report = run_phases(
        &base,
        &[Phase::Write, Phase::Read, Phase::Stat, Phase::Del],
        &sh,
    )
    .await
    .expect("full phase set");
    let got: Vec<Phase> = report.phases.iter().map(|p| p.phase).collect();
    assert_eq!(
        got,
        vec![Phase::Write, Phase::Read, Phase::Stat, Phase::Del],
        "phases must execute and report in the fixed order"
    );
    assert_sane_result(&report.phases[0], data_bytes, io_ops);
    assert_sane_result(&report.phases[1], data_bytes, io_ops);
    assert_sane_result(&report.phases[2], 0, meta_ops);
    assert_sane_result(&report.phases[3], 0, meta_ops);

    // --del doubles as cleanup: the whole dataset tree is gone.
    assert!(
        !dataset_root(&base).exists(),
        "--del must remove the dataset (doubles as cleanup)"
    );
    cleanup(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_read_missing_dataset_is_loud_and_creates_nothing() {
    let base = scratch("read_missing");
    let sh = shape(2, 3, 64 * 1024, 16 * 1024);
    let err = run_phases(&base, &[Phase::Read], &sh)
        .await
        .expect_err("read phase against a missing dataset must fail loudly");
    assert!(
        matches!(err, BenchError::Dataset(_)),
        "must be a dataset-shape error, got: {err:?}"
    );
    assert!(
        !dataset_root(&base).exists(),
        "a read phase must never create files"
    );
    cleanup(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_del_on_mismatched_dataset_deletes_nothing() {
    // Validation happens BEFORE timing/execution: a mismatched dataset
    // must survive an attempted --del untouched.
    let base = scratch("del_mismatch");
    let sh = shape(1, 2, 32 * 1024, 16 * 1024);
    run_phases(&base, &[Phase::Write], &sh)
        .await
        .expect("write phase");
    let victim = bench_file_path(&base, 0, 0);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&victim)
        .expect("open victim")
        .set_len(999)
        .expect("shrink victim");

    let err = run_phases(&base, &[Phase::Del], &sh)
        .await
        .expect_err("del against a mismatched dataset must refuse");
    assert!(matches!(err, BenchError::Dataset(_)));
    assert!(
        victim.exists() && bench_file_path(&base, 0, 1).exists(),
        "nothing may be deleted when validation fails"
    );
    cleanup(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stat_del_without_write_reuse_dataset() {
    let base = scratch("stat_del");
    let sh = shape(2, 2, 16 * 1024, 16 * 1024);
    run_phases(&base, &[Phase::Write], &sh)
        .await
        .expect("write phase");
    let report = run_phases(&base, &[Phase::Stat, Phase::Del], &sh)
        .await
        .expect("stat+del on existing dataset");
    assert_eq!(report.phases.len(), 2);
    assert_eq!(report.phases[0].phase, Phase::Stat);
    assert_eq!(report.phases[1].phase, Phase::Del);
    assert!(!dataset_root(&base).exists());
    cleanup(&base);
}

// ---------------------------------------------------------------------------
// the user's literal scenario: write once, re-read the SAME dataset at
// two different block sizes without rewriting
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_reread_same_dataset_with_different_block_sizes() {
    let base = scratch("reread");
    let write_shape = shape(2, 2, 64 * 1024, 16 * 1024);
    run_phases(&base, &[Phase::Write], &write_shape)
        .await
        .expect("write phase");

    for read_block in [8 * 1024u64, 32 * 1024] {
        let read_shape = shape(2, 2, 64 * 1024, read_block);
        let report = run_phases(&base, &[Phase::Read], &read_shape)
            .await
            .unwrap_or_else(|e| panic!("re-read at block={read_block} must work: {e}"));
        let res = &report.phases[0];
        assert_eq!(res.bytes, 2 * 2 * 64 * 1024, "block={read_block}: bytes");
        assert_eq!(
            res.ops,
            2 * 2 * (64 * 1024 / read_block),
            "block={read_block}: ops"
        );
    }
    // Dataset still present afterwards — reads never mutate it.
    validate_dataset(&base, &write_shape).expect("dataset survives re-reads");
    cleanup(&base);
}

// ---------------------------------------------------------------------------
// --rand: shuffled full-coverage writes + reads (partial tail included)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_rand_write_full_coverage_content() {
    let base = scratch("rand_write");
    let mut sh = shape(2, 2, 64 * 1024, 16 * 1024);
    sh.rand = true;
    run_phases(&base, &[Phase::Write], &sh)
        .await
        .expect("rand write phase");
    for tid in 0..2 {
        for fid in 0..2 {
            // Every block matches its pattern => every block written
            // exactly once (holes would read back zeroed).
            assert_file_matches_pattern(&base, &sh, tid, fid);
        }
    }
    // Rand read over the same dataset covers every block once.
    let report = run_phases(&base, &[Phase::Read], &sh)
        .await
        .expect("rand read phase");
    assert_eq!(report.phases[0].bytes, 2 * 2 * 64 * 1024);
    assert_eq!(report.phases[0].ops, 2 * 2 * 4);
    cleanup(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_eof_partial_block_write_and_read() {
    // size % block != 0: the final op is a partial block (20k = 16k + 4k).
    let base = scratch("partial");
    for rand in [false, true] {
        let mut sh = shape(1, 2, 20 * 1024, 16 * 1024);
        sh.rand = rand;
        let report = run_phases(&base, &[Phase::Write, Phase::Read], &sh)
            .await
            .unwrap_or_else(|e| panic!("partial-tail run (rand={rand}) must work: {e}"));
        for res in &report.phases {
            assert_eq!(
                res.bytes,
                2 * 20 * 1024,
                "rand={rand} {:?} bytes",
                res.phase
            );
            assert_eq!(res.ops, 2 * 2, "rand={rand} {:?} ops", res.phase);
        }
        for fid in 0..2 {
            let p = bench_file_path(&base, 0, fid);
            assert_eq!(
                std::fs::metadata(&p).expect("file exists").len(),
                20 * 1024,
                "partial tail must not round the file size up"
            );
            assert_file_matches_pattern(&base, &sh, 0, fid);
        }
        run_phases(&base, &[Phase::Del], &sh)
            .await
            .expect("cleanup del");
    }
    cleanup(&base);
}

// ---------------------------------------------------------------------------
// write iterations overwrite in place; -w trims stale larger files
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_write_is_authoritative_over_stale_larger_files() {
    let base = scratch("stale");
    let big = shape(1, 1, 64 * 1024, 16 * 1024);
    run_phases(&base, &[Phase::Write], &big)
        .await
        .expect("initial big write");
    // Re-write with a smaller shape: -w must leave an exactly-shaped
    // dataset (no stale tail), so a follow-up read validates.
    let small = shape(1, 1, 32 * 1024, 16 * 1024);
    run_phases(&base, &[Phase::Write], &small)
        .await
        .expect("overwrite with smaller shape");
    assert_eq!(
        std::fs::metadata(bench_file_path(&base, 0, 0))
            .expect("file exists")
            .len(),
        32 * 1024,
        "-w must be authoritative for the dataset shape"
    );
    validate_dataset(&base, &small).expect("dataset matches the new shape");
    run_phases(&base, &[Phase::Read], &small)
        .await
        .expect("read after reshape");
    cleanup(&base);
}

// ---------------------------------------------------------------------------
// O_DIRECT end-to-end (gated: skip cleanly where the scratch fs lacks it)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_direct_end_to_end_when_supported() {
    let base = scratch("direct");
    // Probe O_DIRECT support on the scratch filesystem first.
    let probe = base.join("direct_probe.bin");
    std::fs::write(&probe, vec![0u8; 4096]).expect("probe file");
    let supported = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT)
            .open(&probe)
            .is_ok()
    };
    let _ = std::fs::remove_file(&probe);
    if !supported {
        eprintln!("[SKIP] scratch filesystem does not support O_DIRECT");
        cleanup(&base);
        return;
    }

    for rand in [false, true] {
        let mut sh = shape(2, 2, 64 * 1024, 16 * 1024);
        sh.direct = true;
        sh.rand = rand;
        let report = run_phases(&base, &[Phase::Write, Phase::Read, Phase::Del], &sh)
            .await
            .unwrap_or_else(|e| panic!("--direct rand={rand} run must work: {e}"));
        assert_eq!(report.phases.len(), 3, "rand={rand}");
        assert_eq!(report.phases[0].bytes, 2 * 2 * 64 * 1024, "rand={rand}");
        assert_eq!(report.phases[1].bytes, 2 * 2 * 64 * 1024, "rand={rand}");
    }
    cleanup(&base);
}

// ---------------------------------------------------------------------------
// run_cli guards
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_run_cli_rejects_zero_iterations() {
    let base = scratch("iter0");
    let sh = shape(1, 1, 16 * 1024, 16 * 1024);
    let err = run_cli(&base, &[Phase::Write], &sh, 0)
        .await
        .expect_err("--iterations 0 must be refused");
    assert!(matches!(err, BenchError::Shape(_)));
    cleanup(&base);
}

// ---------------------------------------------------------------------------
// CLI surface: old flags DELETED (clap rejects), new surface accepted
// ---------------------------------------------------------------------------

#[rstest]
#[case::large_size(&["--large-size", "1"])]
#[case::small_size(&["--small-size", "4"])]
#[case::small_count(&["--small-count", "1"])]
#[case::only(&["--only", "metadata"])]
#[case::skip(&["--skip", "metadata"])]
#[case::small_only(&["--small-only"])]
#[case::large_only(&["--large-only"])]
fn test_cli_old_flags_are_rejected(#[case] old_args: &[&str]) {
    let base = scratch(&format!("oldflag_{}", old_args[0].trim_start_matches('-')));
    let out = Command::new(bin())
        .arg("bench")
        .arg(&base)
        .args(old_args)
        .output()
        .expect("spawn squeezefs bench");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "old flag {old_args:?} must be rejected (clean break), got success.\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("error:"),
        "old flag {old_args:?} must produce a clap error, got: {stderr}"
    );
    assert!(
        !dataset_root(&base).exists(),
        "a rejected invocation must not touch the filesystem"
    );
    cleanup(&base);
}

#[test]
fn test_cli_new_surface_end_to_end() {
    let base = scratch("cli_e2e");
    let out = Command::new(bin())
        .arg("bench")
        .arg(&base)
        .args([
            "-w", "-r", "--stat", "--del", "-t", "2", "-n", "3", "-s", "64k", "-b", "16k", "-i",
            "1",
        ])
        .output()
        .expect("spawn squeezefs bench");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "new surface must run: stdout={stdout}\nstderr={stderr}"
    );
    // Per-phase rows in the results table.
    for label in ["Write", "Read", "Stat", "Del"] {
        assert!(
            stdout.contains(label),
            "results table must contain a {label} row.\nstdout: {stdout}"
        );
    }
    assert!(
        !dataset_root(&base).exists(),
        "--del ran last: dataset must be cleaned up"
    );
    cleanup(&base);
}

#[test]
fn test_cli_read_without_dataset_fails_loud_nonzero() {
    let base = scratch("cli_read_missing");
    let out = Command::new(bin())
        .arg("bench")
        .arg(&base)
        .args(["-r", "-t", "2", "-n", "3", "-s", "64k"])
        .output()
        .expect("spawn squeezefs bench");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "read phase against a missing dataset must exit nonzero.\noutput: {combined}"
    );
    assert!(
        combined.contains("-w"),
        "error must tell the user to run -w first.\noutput: {combined}"
    );
    assert!(
        !dataset_root(&base).exists(),
        "nothing may be created by a failed read phase"
    );
    cleanup(&base);
}

#[test]
fn test_cli_direct_unaligned_block_rejected() {
    let base = scratch("cli_direct");
    // Acceptance scenario: `--direct -b 100k` refused. 100k is 4096-aligned,
    // so with -s 1m (not a 100k multiple) the -s % -b rule fires.
    let out = Command::new(bin())
        .arg("bench")
        .arg(&base)
        .args(["-w", "--direct", "-s", "1m", "-b", "100k"])
        .output()
        .expect("spawn squeezefs bench");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "--direct -b 100k must be rejected.\noutput: {combined}"
    );
    assert!(
        combined.contains("multiple"),
        "error must explain the alignment/multiple requirement.\noutput: {combined}"
    );
    assert!(!dataset_root(&base).exists());
    cleanup(&base);

    // And the truly-unaligned block case trips the 4096 rule.
    let out = Command::new(bin())
        .arg("bench")
        .arg(&base)
        .args(["-w", "--direct", "-s", "100k", "-b", "10k"])
        .output()
        .expect("spawn squeezefs bench");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "--direct -b 10k must be rejected.\noutput: {combined}"
    );
    assert!(
        combined.contains("4096"),
        "error must mention 4096 alignment.\noutput: {combined}"
    );
    cleanup(&base);
}

#[test]
fn test_cli_help_documents_fsync_and_units() {
    let out = Command::new(bin())
        .args(["bench", "--help"])
        .output()
        .expect("spawn squeezefs bench --help");
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("fsync"),
        "--help must note that write timing includes fsync (durable numbers).\nhelp: {help}"
    );
    for flag in [
        "--write",
        "--read",
        "--stat",
        "--del",
        "--threads",
        "--files",
        "--size",
        "--block",
        "--rand",
        "--direct",
        "--iterations",
    ] {
        assert!(help.contains(flag), "--help must document {flag}: {help}");
    }
    for gone in ["--large-size", "--small-size", "--small-count", "--only"] {
        assert!(
            !help.contains(gone),
            "--help must not mention deleted flag {gone}"
        );
    }
}
