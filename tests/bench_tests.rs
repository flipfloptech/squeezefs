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
    auto_file_size, auto_total_bytes, bench_file_path, block_count, block_order, block_seed,
    clamp_auto_threads, dataset_root, fill_block, mount_free_bytes, parse_size, phase_passes,
    resolve_shape, resolve_time_box, run_invocation, run_passes, run_phases, select_mode,
    suite_passes, validate_dataset, validate_shape, BenchError, BenchInvocation, BenchMode, Pass,
    Phase, PhaseResult, Shape, AUTO_MIN_TOTAL_BYTES, AUTO_PER_THREAD_BYTES, AUTO_TOTAL_FLOOR_BYTES,
    DEFAULT_BLOCK, DEFAULT_RAND_TIME_BOX_SECS, SUITE_RAND_BLOCK, SUITE_SEQ_BLOCK,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

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
// mode selection: NO phase flags => the full saturation suite (the old
// write+read default is DELETED); any flags => those phases, fixed order
// ---------------------------------------------------------------------------

#[test]
fn test_bare_invocation_selects_the_saturation_suite() {
    assert_eq!(
        select_mode(false, false, false, false),
        BenchMode::Suite,
        "no phase flags must select the full saturation suite (the old \
         write+read default is gone — clean break)"
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
        select_mode(w, r, s, d),
        BenchMode::Phases(expected),
        "phase order is fixed (write, read, stat, del) regardless of flag order"
    );
}

// ---------------------------------------------------------------------------
// auto-shape math: parallelism clamp, 16 GiB floor / 2 GiB×threads,
// 25%-of-free cap, 4 GiB loud error, 1 MiB rounding
// ---------------------------------------------------------------------------

#[rstest]
#[case(0, 1)] // defensive floor
#[case(1, 1)]
#[case(8, 8)]
#[case(16, 16)]
#[case(17, 16)]
#[case(32, 16)]
fn test_auto_threads_clamp(#[case] available: usize, #[case] expected: usize) {
    assert_eq!(clamp_auto_threads(available), expected);
}

#[test]
fn test_auto_total_floor_and_per_thread_scaling() {
    let huge_free = 100 * 1024 * GIB;
    // Small thread counts hit the 16 GiB floor.
    assert_eq!(
        auto_total_bytes(4, huge_free).expect("4 threads"),
        AUTO_TOTAL_FLOOR_BYTES,
        "total = max(16 GiB, 2 GiB × 4) = 16 GiB"
    );
    // Large thread counts scale at 2 GiB per thread.
    assert_eq!(
        auto_total_bytes(16, huge_free).expect("16 threads"),
        16 * AUTO_PER_THREAD_BYTES,
        "total = max(16 GiB, 2 GiB × 16) = 32 GiB"
    );
}

#[test]
fn test_auto_total_caps_at_quarter_of_free_space() {
    // 40 GiB free => cap 10 GiB < the 16 GiB floor => capped total.
    assert_eq!(
        auto_total_bytes(4, 40 * GIB).expect("capped total"),
        10 * GIB
    );
    // A non-aligned free space still yields a 1 MiB-multiple total
    // (so -s % -b == 0 holds for both the 1m and 4k suite passes).
    let total = auto_total_bytes(4, 40 * GIB + 123_456_789).expect("odd free");
    assert_eq!(total % MIB, 0, "auto totals must round down to 1 MiB");
    assert!((10 * GIB..10 * GIB + 32 * MIB).contains(&total));
}

#[test]
fn test_auto_total_too_small_filesystem_is_loud() {
    // 8 GiB free => 25% cap = 2 GiB < the 4 GiB minimum => loud error.
    let err = auto_total_bytes(4, 8 * GIB).expect_err("2 GiB cap must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("4 GiB") && msg.contains("25%"),
        "error must state the 4 GiB minimum and the 25% free-space cap, got: {msg}"
    );
    // Exactly 16 GiB free => cap = exactly 4 GiB => allowed.
    assert_eq!(
        auto_total_bytes(1, 16 * GIB).expect("4 GiB fits exactly"),
        AUTO_MIN_TOTAL_BYTES
    );
    // Just below => refused.
    assert!(auto_total_bytes(1, 16 * GIB - 4 * MIB).is_err());
}

#[test]
fn test_auto_file_size_rounds_down_to_1mib() {
    assert_eq!(
        auto_file_size(32 * GIB, 16, 1).expect("even split"),
        2 * GIB
    );
    assert_eq!(auto_file_size(16 * GIB, 4, 2).expect("files>1"), 2 * GIB);
    // Non-aligned per-file result rounds down to a 1 MiB multiple.
    assert_eq!(
        auto_file_size(3 * MIB + 123, 1, 1).expect("odd total"),
        3 * MIB
    );
    let sz = auto_file_size(10 * GIB + 999, 3, 1).expect("odd split");
    assert_eq!(sz % MIB, 0);
    // Per-file below 1 MiB => loud error, never a zero-byte shape.
    let err = auto_file_size(4 * GIB, 16, 1024).expect_err("tiny per-file");
    assert!(
        err.to_string().contains("1 MiB"),
        "error must explain the 1 MiB rounding floor, got: {err}"
    );
}

#[test]
fn test_resolve_shape_explicit_flags_always_override() {
    let base = scratch("resolve_explicit");
    let r = resolve_shape(&base, Some(3), Some(2), Some(64 * MIB), Some(4096))
        .expect("explicit resolve");
    assert_eq!(
        (r.threads, r.files, r.size, r.block),
        (3, 2, 64 * MIB, 4096)
    );
    assert!(
        !r.threads_auto && !r.files_auto && !r.size_auto && !r.block_auto,
        "explicit flags must be marked explicit for the header"
    );
    cleanup(&base);
}

#[test]
fn test_resolve_shape_auto_defaults() {
    let base = scratch("resolve_auto");
    // Explicit size => no statfs dependency; the rest auto-resolve.
    let r = resolve_shape(&base, None, None, Some(8 * MIB), None).expect("auto resolve");
    let expect_threads = clamp_auto_threads(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    );
    assert_eq!(r.threads, expect_threads, "threads auto = min(CPUs, 16)");
    assert!(r.threads_auto);
    assert_eq!(r.files, 1, "files auto = 1 per thread");
    assert!(r.files_auto);
    assert_eq!(r.block, DEFAULT_BLOCK, "block defaults to 1m");
    assert!(r.block_auto);
    assert!(!r.size_auto);

    // Auto size matches the pure composition over the real statfs free
    // space (tolerate free-space jitter between the two statfs calls).
    let free = mount_free_bytes(&base).expect("statfs");
    if free >= 68 * GIB {
        let r2 = resolve_shape(&base, Some(2), None, None, None).expect("auto size");
        assert!(r2.size_auto);
        let expected =
            auto_file_size(auto_total_bytes(2, free).expect("total"), 2, 1).expect("per-file");
        let diff = r2.size.abs_diff(expected);
        assert!(
            diff <= 64 * MIB,
            "auto size must follow max(16g,2g×t) capped at 25% free: got {} vs {expected}",
            r2.size
        );
        assert_eq!(r2.size % MIB, 0, "auto size must be a 1 MiB multiple");
    } else {
        eprintln!("[SKIP] scratch fs has < 68 GiB free; exact auto-size pin skipped");
    }
    cleanup(&base);
}

// ---------------------------------------------------------------------------
// --time semantics: rand default 30 s, seq unlimited, 0 = full coverage,
// explicit override
// ---------------------------------------------------------------------------

#[rstest]
#[case(None, false, None)]
#[case(None, true, Some(Duration::from_secs(DEFAULT_RAND_TIME_BOX_SECS)))]
#[case(Some(0), true, None)] // --time 0 forces full coverage on rand
#[case(Some(0), false, None)]
#[case(Some(7), true, Some(Duration::from_secs(7)))]
#[case(Some(7), false, Some(Duration::from_secs(7)))]
fn test_resolve_time_box(
    #[case] time: Option<u64>,
    #[case] rand: bool,
    #[case] expected: Option<Duration>,
) {
    assert_eq!(
        resolve_time_box(time, rand),
        expected,
        "time={time:?} rand={rand}"
    );
}

// ---------------------------------------------------------------------------
// the saturation suite: pass sequence, per-pass shapes, time boxes,
// direct ON for I/O passes, one dataset lifecycle ending clean
// ---------------------------------------------------------------------------

fn assert_suite_shape(passes: &[Pass], threads: usize, files: usize, size: u64) {
    for (i, p) in passes.iter().enumerate() {
        assert_eq!(
            (p.shape.threads, p.shape.files, p.shape.size),
            (threads, files, size),
            "pass {i}: ONE dataset shape across the whole suite"
        );
    }
}

#[test]
fn test_suite_pass_sequence_and_shapes() {
    let passes = suite_passes(2, 1, 8 * MIB, None);
    assert_eq!(passes.len(), 6, "suite = 6 passes");
    assert_suite_shape(&passes, 2, 1, 8 * MIB);

    let seq: Vec<(Phase, u64, bool, Option<Duration>, bool)> = passes
        .iter()
        .map(|p| (p.phase, p.shape.block, p.shape.rand, p.time_box, p.validate))
        .collect();
    let box30 = Some(Duration::from_secs(DEFAULT_RAND_TIME_BOX_SECS));
    assert_eq!(
        seq[0],
        (Phase::Write, SUITE_SEQ_BLOCK, false, None, false),
        "pass 1: write seq 1m, full coverage, creates the dataset"
    );
    assert_eq!(
        seq[1],
        (Phase::Read, SUITE_SEQ_BLOCK, false, None, true),
        "pass 2: read seq 1m, validates pass 1's dataset"
    );
    assert_eq!(
        seq[2],
        (Phase::Read, SUITE_RAND_BLOCK, true, box30, true),
        "pass 3: read rand 4k, 30 s box"
    );
    assert_eq!(
        seq[3],
        (Phase::Write, SUITE_RAND_BLOCK, true, box30, true),
        "pass 4: write rand 4k, 30 s box, over the EXISTING dataset"
    );
    assert_eq!(seq[4].0, Phase::Stat, "pass 5: stat");
    assert!(seq[4].4, "stat validates");
    assert_eq!(seq[5].0, Phase::Del, "pass 6: del (leaves the mount clean)");
    assert!(seq[5].4, "del validates");

    // All four I/O passes are O_DIRECT in the suite.
    for (i, p) in passes.iter().take(4).enumerate() {
        assert!(p.shape.direct, "suite I/O pass {i} must be O_DIRECT");
    }
}

#[test]
fn test_suite_time_flag_overrides_rand_boxes_only() {
    let passes = suite_passes(1, 1, 4 * MIB, Some(10));
    assert_eq!(passes[0].time_box, None, "seq write is never time-boxed");
    assert_eq!(passes[1].time_box, None, "seq read is never time-boxed");
    assert_eq!(passes[2].time_box, Some(Duration::from_secs(10)));
    assert_eq!(passes[3].time_box, Some(Duration::from_secs(10)));

    // --time 0 forces full coverage on the rand passes.
    let full = suite_passes(1, 1, 4 * MIB, Some(0));
    assert_eq!(full[2].time_box, None);
    assert_eq!(full[3].time_box, None);
}

// ---------------------------------------------------------------------------
// consistency rule: a single-phase invocation inherits the identical
// defaults (same shape resolution, same 30 s rand box) as the suite pass
// ---------------------------------------------------------------------------

#[test]
fn test_phase_passes_inherit_rand_time_box_default() {
    let mut sh = shape(1, 1, 4 * MIB, 4096);
    sh.rand = true;
    let passes = phase_passes(&[Phase::Read], &sh, None);
    assert_eq!(passes.len(), 1);
    assert_eq!(
        passes[0].time_box,
        Some(Duration::from_secs(DEFAULT_RAND_TIME_BOX_SECS)),
        "single-phase rand run must inherit the suite's 30 s box (comparable numbers)"
    );
    assert!(
        passes[0].validate,
        "read without write validates the dataset first"
    );

    // Sequential single-phase: unlimited (full coverage).
    let seq = phase_passes(&[Phase::Read], &shape(1, 1, 4 * MIB, MIB), None);
    assert_eq!(seq[0].time_box, None);

    // Write present => no validation anywhere; stat/del never boxed.
    let wrd = phase_passes(
        &[Phase::Write, Phase::Read, Phase::Stat, Phase::Del],
        &shape(1, 1, 4 * MIB, MIB),
        Some(5),
    );
    assert!(wrd.iter().all(|p| !p.validate));
    assert_eq!(wrd[0].time_box, Some(Duration::from_secs(5)));
    assert_eq!(wrd[1].time_box, Some(Duration::from_secs(5)));
    assert_eq!(wrd[2].time_box, None, "stat always completes");
    assert_eq!(wrd[3].time_box, None, "del always completes");
}

// ---------------------------------------------------------------------------
// time-boxed execution: partial coverage, >=1 op per worker, honest rows
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_time_box_expiry_yields_partial_coverage() {
    let base = scratch("timebox");
    let mut wr = shape(2, 2, 64 * 1024, 4096);
    run_phases(&base, &[Phase::Write], &wr)
        .await
        .expect("dataset");

    wr.rand = true;
    let pass = Pass {
        phase: Phase::Read,
        shape: wr.clone(),
        time_box: Some(Duration::ZERO),
        validate: true,
    };
    let report = run_passes(&base, &[pass]).await.expect("boxed read");
    let res = &report.phases[0];
    let expected_full = 2 * 2 * (64 * 1024 / 4096);
    assert_eq!(res.expected_ops, expected_full);
    assert!(
        res.ops >= 2 && res.ops < expected_full,
        "zero box: at least one op per worker, well short of full coverage (got {})",
        res.ops
    );
    assert!(res.coverage() < 1.0, "coverage must be partial");
    assert_eq!(res.time_box, Some(Duration::ZERO));
    assert!(res.iops() > 0.0);

    // Time-boxed rand WRITE over the existing dataset: sizes untouched
    // (overwrite in place), dataset still shape-valid afterwards.
    let wpass = Pass {
        phase: Phase::Write,
        shape: wr.clone(),
        time_box: Some(Duration::ZERO),
        validate: true,
    };
    let report = run_passes(&base, &[wpass]).await.expect("boxed write");
    assert!(report.phases[0].ops >= 2);
    assert!(report.phases[0].coverage() < 1.0);
    validate_dataset(&base, &wr).expect("boxed rand write must leave the dataset shape-valid");
    cleanup(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_suite_lifecycle_on_tiny_dataset() {
    // The suite's pass structure over a tiny dataset (buffered variant so
    // it runs on any scratch filesystem; the O_DIRECT flavor is gated
    // below): one dataset, six passes, mount left clean.
    let base = scratch("suite_tiny");
    let mut passes = suite_passes(2, 1, 8 * MIB, None);
    for p in &mut passes {
        p.shape.direct = false;
    }
    let report = run_passes(&base, &passes).await.expect("tiny suite");
    let got: Vec<Phase> = report.phases.iter().map(|p| p.phase).collect();
    assert_eq!(
        got,
        vec![
            Phase::Write,
            Phase::Read,
            Phase::Read,
            Phase::Write,
            Phase::Stat,
            Phase::Del
        ],
        "suite pass order"
    );
    assert_eq!(report.phases[2].block, SUITE_RAND_BLOCK);
    assert_eq!(report.phases[3].block, SUITE_RAND_BLOCK);
    for res in &report.phases {
        assert!(res.ops > 0, "{:?}: ops", res.phase);
        assert!(
            res.coverage() >= 1.0 - f64::EPSILON,
            "tiny dataset finishes well inside the 30 s boxes"
        );
        assert!(
            res.lat_min <= res.lat_avg && res.lat_avg <= res.lat_max,
            "{:?}: monotonic latencies",
            res.phase
        );
    }
    assert!(
        !dataset_root(&base).exists(),
        "the suite must leave the mount clean (del is the last pass)"
    );
    cleanup(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_suite_lifecycle_direct_when_supported() {
    let base = scratch("suite_direct");
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
    let passes = suite_passes(2, 1, 8 * MIB, None);
    let report = run_passes(&base, &passes).await.expect("direct suite");
    assert_eq!(report.phases.len(), 6);
    assert!(report.phases.iter().take(4).all(|r| r.direct));
    assert!(!dataset_root(&base).exists());
    cleanup(&base);
}

// ---------------------------------------------------------------------------
// run_invocation guards: suite refuses -b/--rand; iterations >= 1
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_suite_refuses_explicit_block_and_rand() {
    let base = scratch("suite_guard");
    let inv = BenchInvocation {
        block: Some(4096),
        size: Some(8 * MIB),
        threads: Some(1),
        iterations: 1,
        ..Default::default()
    };
    let err = run_invocation(&base, &inv)
        .await
        .expect_err("-b without phases must be refused (suite fixes per-pass blocks)");
    assert!(
        matches!(&err, BenchError::Shape(m) if m.contains("-b")),
        "error must point at -b vs the suite, got: {err}"
    );

    let inv = BenchInvocation {
        rand: true,
        size: Some(8 * MIB),
        threads: Some(1),
        iterations: 1,
        ..Default::default()
    };
    let err = run_invocation(&base, &inv)
        .await
        .expect_err("--rand without phases must be refused");
    assert!(matches!(&err, BenchError::Shape(m) if m.contains("--rand")));
    assert!(
        !dataset_root(&base).exists(),
        "guard failures must not touch the filesystem"
    );
    cleanup(&base);
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
// run_invocation guards
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_run_invocation_rejects_zero_iterations() {
    let base = scratch("iter0");
    let inv = BenchInvocation {
        write: true,
        threads: Some(1),
        size: Some(16 * 1024),
        block: Some(16 * 1024),
        iterations: 0,
        ..Default::default()
    };
    let err = run_invocation(&base, &inv)
        .await
        .expect_err("--iterations 0 must be refused");
    assert!(matches!(err, BenchError::Shape(_)));
    cleanup(&base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_single_phase_inherits_shape_against_written_dataset() {
    // The user flow the consistency rule protects: -w with an explicit
    // tiny shape, then a lone `-r --rand -b 4k` (the suite pass-3 shape)
    // against the surviving dataset.
    let base = scratch("consistency");
    let winv = BenchInvocation {
        write: true,
        threads: Some(2),
        size: Some(8 * MIB),
        iterations: 1,
        ..Default::default()
    };
    run_invocation(&base, &winv).await.expect("write phase");

    let rinv = BenchInvocation {
        read: true,
        rand: true,
        block: Some(4096),
        threads: Some(2),
        size: Some(8 * MIB),
        iterations: 1,
        ..Default::default()
    };
    run_invocation(&base, &rinv)
        .await
        .expect("single-phase rand 4k read over the -w dataset");

    // Cleanup via the del phase.
    let dinv = BenchInvocation {
        del: true,
        threads: Some(2),
        size: Some(8 * MIB),
        iterations: 1,
        ..Default::default()
    };
    run_invocation(&base, &dinv).await.expect("del phase");
    assert!(!dataset_root(&base).exists());
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

    // And the truly-unaligned block case trips the 4096 rule (fresh
    // scratch dir: the benchmark path must exist — path errors come
    // before shape errors).
    let base = scratch("cli_direct_4096");
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
fn test_cli_help_documents_fsync_units_time_and_auto() {
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
        "--time",
        "--iterations",
    ] {
        assert!(help.contains(flag), "--help must document {flag}: {help}");
    }
    assert!(
        help.contains("auto"),
        "--help must state that -t/-n/-s auto-size by default.\nhelp: {help}"
    );
    assert!(
        help.to_ascii_lowercase().contains("suite"),
        "--help must describe the bare-invocation saturation suite.\nhelp: {help}"
    );
    for gone in ["--large-size", "--small-size", "--small-count", "--only"] {
        assert!(
            !help.contains(gone),
            "--help must not mention deleted flag {gone}"
        );
    }
}

#[test]
fn test_cli_bare_invocation_runs_suite_with_explicit_tiny_shape() {
    let base = scratch("cli_suite");
    // Gate: the suite's I/O passes are O_DIRECT; skip cleanly where the
    // scratch filesystem cannot do it.
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

    // No phase flags => the full suite; explicit -t/-s keep it tiny (auto
    // sizing would want >= 16 GiB — that lives in the real-mount smoke).
    let out = Command::new(bin())
        .arg("bench")
        .arg(&base)
        .args(["-t", "1", "-s", "8m"])
        .output()
        .expect("spawn squeezefs bench");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bare invocation must run the suite.\nstdout: {stdout}\nstderr: {stderr}"
    );
    // The old write+read default is gone: all six passes show up.
    for label in ["Write", "Read", "Stat", "Del"] {
        assert!(stdout.contains(label), "suite must include {label} rows");
    }
    assert!(
        stdout.contains("rand") && stdout.contains("seq"),
        "suite rows must distinguish seq and rand passes.\nstdout: {stdout}"
    );
    // The header states the computed shape with auto-vs-explicit
    // provenance.
    assert!(
        stdout.contains("(explicit)") && stdout.contains("(auto)"),
        "header must mark auto vs explicit values.\nstdout: {stdout}"
    );
    assert!(
        !dataset_root(&base).exists(),
        "the suite ends with del: mount left clean"
    );
    cleanup(&base);
}

#[test]
fn test_cli_block_without_phases_is_refused() {
    let base = scratch("cli_suite_block");
    let out = Command::new(bin())
        .arg("bench")
        .arg(&base)
        .args(["-b", "4k", "-t", "1", "-s", "8m"])
        .output()
        .expect("spawn squeezefs bench");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "-b without phase flags must be refused (the suite fixes per-pass blocks).\n{combined}"
    );
    assert!(
        combined.contains("-b"),
        "error must explain the -b/suite conflict.\n{combined}"
    );
    assert!(!dataset_root(&base).exists());
    cleanup(&base);
}
