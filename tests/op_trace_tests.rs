//! E2E performance-audit PR A2 — the per-op TRACE RING contracts
//! (`docs/design-e2e-perf-audit.md` §1 honesty precondition 2, Appendix
//! D item 2). Every cross-layer number the notes quote was a SUBTRACTION
//! of independent histograms (`fio clat − transport_total`,
//! `clat − ipc_direct total`); these contracts pin the instrument that
//! joins the stages of ONE op on one timeline.
//!
//! Laws (never perf numbers — the bench rows carry those):
//!
//! 1. **Disarmed hooks are inert**: no samples, no allocation, no clock
//!    read; `stamp`/`stamp_current`/`OpScope` are one relaxed load (or a
//!    plain field compare) when the ring is not armed.
//! 2. **op_id law**: the FUSE request `unique` IS the op_id for kernel
//!    ops (the kernel's `fuse_request_send/end` tracepoints carry it —
//!    the join key); il ring ops use the slot ticket
//!    `(session, slot, generation)` under the IL namespace bit; conveyor
//!    work carries the ORIGINATING op's id through the tx.
//! 3. **Ordered chains**: an armed kernel READ / WRITE through the FUSE
//!    fixture yields a monotone stage chain for its op_id; a create
//!    yields the meta_op + conveyor chain carrying the same op_id.
//! 4. **Overflow drops and counts, never blocks or allocates**.
//! 5. **`.trace` read DRAINS** (a second read is empty); export shape
//!    pinned; VAL-7a owner-only mode pinned.
//! 6. **`SQUEEZEFS_OP_TRACE`** is a registered Bool knob (ENG-10).

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::op_trace::{self, Sample, Stage};
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;
use tempfile::{tempdir, NamedTempFile, TempDir};

/// Downscaled block: the striped write path with a small fixture.
const BS: u64 = 65536;

/// The trace ring is process-global; every test serializes and leaves
/// the ring DISARMED and drained.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

struct Armed {
    /// Held for the test's whole body (RAII serialization).
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for Armed {
    fn drop(&mut self) {
        op_trace::disarm();
        let _ = op_trace::drain();
    }
}

async fn serial() -> Armed {
    squeezefs::mem_budget::MEM_BUDGET.set_flag_budget(1 << 30);
    squeezefs::mem_budget::MEM_BUDGET.tick();
    let g = SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    op_trace::disarm();
    let _ = op_trace::drain();
    Armed { _guard: g }
}

struct H {
    fs: SqueezefsFilesystem,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

/// The audit_instruments harness: real v3 meta backend, real striped
/// write path, real publish + journal conveyors.
async fn make(uuid: [u8; 16], alloc_ns: &str) -> H {
    std::env::set_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE", BS.to_string());
    // Request-driven reads only (the read_serve_phase_tests posture): no
    // R2 pipeline fetches / R3 ranged windows — their fills would add
    // device stamps under NO op id and blur the chain assertions.
    std::env::set_var("SQUEEZEFS_READ_PREFETCH_WINDOW", "0");
    std::env::set_var("SQUEEZEFS_READ_RANGED_THRESHOLD", "0");
    squeezefs::fuse_client::set_patch_max_bytes(0);
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("64MB"),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    ImageBuilder::new(BuilderConfig {
        node_size: DEFAULT_NODE_SIZE,
        journal_len_override: None,
        hash_seed: 0xC0FF_EE00_1234_5678,
        uuid,
    })
    .unwrap()
    .build(m.path(), 128 * 1024 * 1024)
    .await
    .unwrap();
    let be = KvMetaBackend::open(m.path()).await.unwrap();
    let routed = Arc::new(RoutedMetaBackend::new(vec![be]));
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    H {
        fs,
        _b: b,
        _m: m,
        _s: s,
    }
}

fn req(unique: u64) -> Request {
    Request {
        unique,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    }
}

fn pattern(len: usize, tag: u8) -> Vec<u8> {
    (0..len).map(|i| (i % 249) as u8 ^ tag | 1).collect()
}

/// The samples of one op, in ring order.
fn chain(samples: &[Sample], op_id: u64) -> Vec<Sample> {
    samples
        .iter()
        .filter(|s| s.op_id == op_id)
        .copied()
        .collect()
}

fn has_stage(chain: &[Sample], stage: Stage) -> bool {
    chain.iter().any(|s| s.stage == stage as u16)
}

fn stage_ns(chain: &[Sample], stage: Stage) -> u64 {
    chain
        .iter()
        .find(|s| s.stage == stage as u16)
        .unwrap_or_else(|| panic!("stage {stage:?} missing from {chain:?}"))
        .mono_ns
}

// ---------------------------------------------------------------------------
// 1 — the ring core: disarmed inertness, arm/drain, selection
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disarmed_hooks_record_nothing() {
    let _g = serial().await;
    assert!(!op_trace::is_armed());
    // The counters are cumulative over the ring's life: deltas.
    let base_total = op_trace::samples_total();
    let base_dropped = op_trace::dropped();
    let now = Instant::now();
    op_trace::stamp(42, Stage::TransportRecv, now);
    op_trace::stamp_now(42, Stage::Dispatch);
    op_trace::stamp_current(Stage::HandlerEntry, now);
    assert_eq!(op_trace::current_op(), 0, "no scope ⇒ no current op");
    // A scope on a disarmed ring binds NOTHING: the hooks inside see 0.
    let seen = op_trace::scope(42, async { op_trace::current_op() }).await;
    assert_eq!(seen, 0, "a disarmed scope never binds an op id");
    assert!(op_trace::drain().is_empty(), "disarmed ⇒ no samples");
    assert_eq!(op_trace::samples_total(), base_total, "no push counted");
    assert_eq!(op_trace::dropped(), base_dropped, "no drop counted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn armed_explicit_stamps_land_sorted_and_drain_empties() {
    let _g = serial().await;
    op_trace::arm_for_tests(1);
    assert!(op_trace::is_armed());
    assert_eq!(op_trace::divisor(), 1, "armed-by-a-test ⇒ every op sampled");
    let t0 = Instant::now();
    let t1 = t0 + std::time::Duration::from_micros(10);
    let t2 = t0 + std::time::Duration::from_micros(20);
    // Out of order on purpose: the drain sorts by (op_id, ns).
    op_trace::stamp(7, Stage::Dispatch, t1);
    op_trace::stamp(3, Stage::TransportRecv, t0);
    op_trace::stamp(7, Stage::TransportRecv, t0);
    op_trace::stamp(7, Stage::HandlerEntry, t2);
    op_trace::stamp(0, Stage::HandlerEntry, t2); // 0 is never an op id
                                                 // Detached tasks of an earlier fixture may still be stamping under
                                                 // their own ids: judge OUR ids only.
    let got: Vec<Sample> = op_trace::drain()
        .into_iter()
        .filter(|s| s.op_id == 3 || s.op_id == 7)
        .collect();
    assert_eq!(got.len(), 4, "four real stamps, the id-0 one refused");
    let ids: Vec<u64> = got.iter().map(|s| s.op_id).collect();
    assert_eq!(ids, vec![3, 7, 7, 7], "sorted by op_id");
    let c7 = chain(&got, 7);
    assert_eq!(
        c7.iter().map(|s| s.stage).collect::<Vec<_>>(),
        vec![
            Stage::TransportRecv as u16,
            Stage::Dispatch as u16,
            Stage::HandlerEntry as u16
        ],
        "within an op, sorted by time"
    );
    assert!(
        c7.windows(2).all(|w| w[0].mono_ns <= w[1].mono_ns),
        "monotone timestamps"
    );
    assert_eq!(
        stage_ns(&c7, Stage::HandlerEntry) - stage_ns(&c7, Stage::TransportRecv),
        20_000,
        "stamps carry the Instant deltas exactly (ns)"
    );
    assert!(
        op_trace::drain()
            .iter()
            .all(|s| s.op_id != 3 && s.op_id != 7),
        "a drain EMPTIES the rings — the second read carries none of ours"
    );
}

/// The divisor law: `traced(op_id)` is a deterministic per-op decision
/// (every hook on one op agrees), samples ≈ 1/N of a dense id stream,
/// and it is stride-independent (FUSE uniques step by
/// `FUSE_REQ_ID_STEP = 2`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sampling_divisor_is_deterministic_and_stride_independent() {
    let _g = serial().await;
    op_trace::arm_for_tests(8);
    assert_eq!(op_trace::divisor(), 8);
    let dense = (1..=8_000u64)
        .filter(|id| op_trace::traced(*id) != 0)
        .count();
    let even = (1..=8_000u64)
        .map(|k| k * 2)
        .filter(|id| op_trace::traced(*id) != 0)
        .count();
    for (label, n) in [("dense", dense), ("even", even)] {
        assert!(
            (600..=1_400).contains(&n),
            "{label}: {n} of 8000 sampled at N=8 (expect ≈ 1000)"
        );
    }
    for id in [5u64, 6, 7, 1_000_003] {
        assert_eq!(
            op_trace::traced(id),
            op_trace::traced(id),
            "deterministic per id"
        );
        let t = op_trace::traced(id);
        assert!(t == 0 || t == id, "traced returns the id or 0");
    }
    // A stamp on an UNSAMPLED id is refused; on a sampled id it lands.
    let sampled = (1..=10_000u64)
        .find(|id| op_trace::traced(*id) != 0)
        .unwrap();
    let unsampled = (1..=10_000u64)
        .find(|id| op_trace::traced(*id) == 0)
        .unwrap();
    let now = Instant::now();
    op_trace::stamp(sampled, Stage::TransportRecv, now);
    op_trace::stamp(unsampled, Stage::TransportRecv, now);
    let got = op_trace::drain();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].op_id, sampled);
}

/// The scope binds the op id across await points and lanes; hooks inside
/// stamp under it; nested scopes shadow; nothing leaks past the poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scope_binds_the_current_op_across_awaits() {
    let _g = serial().await;
    op_trace::arm_for_tests(1);
    let (outer, inner, outer_again) = op_trace::scope(11, async {
        let a = op_trace::current_op();
        tokio::task::yield_now().await;
        op_trace::stamp_current(Stage::MetaBackendStart, Instant::now());
        let b = op_trace::scope(12, async {
            tokio::task::yield_now().await;
            op_trace::stamp_current(Stage::MetaBackendDone, Instant::now());
            op_trace::current_op()
        })
        .await;
        let c = op_trace::current_op();
        (a, b, c)
    })
    .await;
    assert_eq!((outer, inner, outer_again), (11, 12, 11));
    assert_eq!(op_trace::current_op(), 0, "unbound outside the scope");
    let got = op_trace::drain();
    assert_eq!(chain(&got, 11).len(), 1);
    assert_eq!(chain(&got, 12).len(), 1);
    assert!(has_stage(&chain(&got, 11), Stage::MetaBackendStart));
    assert!(has_stage(&chain(&got, 12), Stage::MetaBackendDone));
}

/// Ring overflow: pushes past the per-thread capacity DROP and COUNT —
/// no block, no allocation, and the samples that did land are intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ring_overflow_drops_and_counts_never_blocks() {
    let _g = serial().await;
    const CAP: usize = 64;
    op_trace::arm_for_tests_with_geometry(1, 4, CAP);
    let now = Instant::now();
    let n = CAP * 3;
    let t0 = Instant::now();
    for i in 0..n as u64 {
        op_trace::stamp(1_000 + i, Stage::TransportRecv, now);
    }
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(500),
        "overflow must never block"
    );
    let dropped = op_trace::dropped();
    assert_eq!(
        dropped,
        (n - CAP) as u64,
        "every push past capacity is counted"
    );
    let got = op_trace::drain();
    assert_eq!(got.len(), CAP, "exactly the ring's capacity landed");
    for s in &got {
        assert_eq!(s.stage, Stage::TransportRecv as u16);
        assert!(
            (1_000..1_000 + n as u64).contains(&s.op_id),
            "intact sample"
        );
    }
    assert_eq!(op_trace::samples_total(), CAP as u64);
    // After a drain the ring accepts again.
    op_trace::stamp(5, Stage::Dispatch, now);
    assert_eq!(op_trace::drain().len(), 1);
}

/// Concurrent producers on their own rings + one drain: every sample
/// lands exactly once, none torn (op_id/stage/ns all agree).
#[test]
fn concurrent_producers_fold_exactly() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _g = rt.block_on(serial());
    const THREADS: u64 = 6;
    const PER: u64 = 2_000;
    op_trace::arm_for_tests_with_geometry(1, 16, 1 << 12);
    let hs: Vec<_> = (0..THREADS)
        .map(|t| {
            std::thread::spawn(move || {
                let base = Instant::now();
                for i in 0..PER {
                    let id = (t << 32) | (i + 1);
                    op_trace::stamp(
                        id,
                        Stage::TransportRecv,
                        base + std::time::Duration::from_nanos(i),
                    );
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let got = op_trace::drain();
    assert_eq!(op_trace::dropped(), 0, "the rings were sized for the load");
    assert_eq!(got.len(), (THREADS * PER) as usize);
    let mut seen = std::collections::HashSet::new();
    for s in &got {
        assert!(seen.insert(s.op_id), "duplicate sample {s:?}");
        assert_eq!(s.stage, Stage::TransportRecv as u16);
    }
}

/// Every stage id has a unique name and round-trips through `from_u16`;
/// the name table is what the export ships.
#[test]
fn stage_table_is_total_and_unique() {
    let mut names = std::collections::HashSet::new();
    for s in Stage::ALL {
        assert_eq!(Stage::from_u16(*s as u16), Some(*s), "{s:?} round-trips");
        assert!(names.insert(s.name()), "duplicate stage name {}", s.name());
        assert!(
            s.name()
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "snake_case names: {}",
            s.name()
        );
    }
    assert_eq!(Stage::from_u16(0), None, "0 is never a stage");
    assert_eq!(Stage::from_u16(u16::MAX), None);
}

// ---------------------------------------------------------------------------
// 2 — the op_id law
// ---------------------------------------------------------------------------

/// il ring ops: the slot ticket `(session, slot, generation)` under the
/// IL namespace bit — disjoint from every kernel `unique` (uniques are
/// small counters; bit 63 is the namespace).
#[test]
fn il_op_id_is_namespaced_and_injective_over_the_ticket() {
    let a = op_trace::il_op_id(1, 0, 1);
    let b = op_trace::il_op_id(1, 0, 2);
    let c = op_trace::il_op_id(1, 1, 1);
    let d = op_trace::il_op_id(2, 0, 1);
    for id in [a, b, c, d] {
        assert_ne!(id & op_trace::OP_ID_IL_BIT, 0, "IL namespace bit set");
        assert_ne!(id, 0);
    }
    let set: std::collections::HashSet<u64> = [a, b, c, d].into_iter().collect();
    assert_eq!(set.len(), 4, "distinct tickets ⇒ distinct ids");
    // A kernel unique never carries the namespace bit.
    assert_eq!(1_000_000u64 & op_trace::OP_ID_IL_BIT, 0);
}

// ---------------------------------------------------------------------------
// 3 — ordered chains through the FUSE fixture
// ---------------------------------------------------------------------------

/// An armed kernel WRITE (the striped write-through venue) yields, for
/// the request's `unique`, the handler-side chain: the pipeline stages
/// with monotone timestamps and the device funnel's submit/complete
/// pair carried by the enqueued request (the worker thread has no scope
/// — the id travels on the request).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn armed_write_yields_an_ordered_pipeline_chain_for_its_unique() {
    let _g = serial().await;
    let h = make([0xA2; 16], "op_trace_write").await;
    let ino =
        h.fs.create(req(2), 1, OsStr::new("w.bin"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    // Promote to striped first (fixture write + fsync), untraced.
    let data = pattern(BS as usize * 4, 0x11);
    let w =
        h.fs.write(req(4), ino, 0, 0, bytes::Bytes::from(data.clone()), 0, 0)
            .await
            .unwrap();
    assert_eq!(w.written as usize, data.len());
    h.fs.fsync(req(6), ino, 0, false).await.unwrap();

    op_trace::arm_for_tests(1);
    const UNIQUE: u64 = 1_000;
    let data2 = pattern(BS as usize * 4, 0x22);
    let w = op_trace::scope(
        UNIQUE,
        h.fs.write(
            req(UNIQUE),
            ino,
            0,
            0,
            bytes::Bytes::from(data2.clone()),
            0,
            0,
        ),
    )
    .await
    .unwrap();
    assert_eq!(w.written as usize, data2.len());
    op_trace::scope(UNIQUE, h.fs.fsync(req(UNIQUE), ino, 0, false))
        .await
        .unwrap();
    // The detached pipeline uploads carry the op id past the handler
    // return; fsync above waited them out.
    let got = op_trace::drain();
    let c = chain(&got, UNIQUE);
    assert!(!c.is_empty(), "the traced write left samples: {got:?}");
    assert!(
        c.windows(2).all(|w| w[0].mono_ns <= w[1].mono_ns),
        "chain is time-ordered: {c:?}"
    );
    for st in [Stage::WriteAdmitted, Stage::WriteDmaDone, Stage::WriteDone] {
        assert!(has_stage(&c, st), "write chain lacks {st:?}: {c:?}");
    }
    assert!(
        has_stage(&c, Stage::DevSubmit) && has_stage(&c, Stage::DevComplete),
        "the device funnel stamps ride the request's trace id: {c:?}"
    );
    assert!(
        stage_ns(&c, Stage::DevSubmit) <= stage_ns(&c, Stage::DevComplete),
        "submit precedes complete"
    );
    assert!(
        stage_ns(&c, Stage::DevComplete) <= stage_ns(&c, Stage::WriteDmaDone),
        "the DMA span brackets the device leg"
    );
    // Untraced ops (the fixture's uniques 2/4/6) left nothing.
    for u in [2u64, 4, 6] {
        assert!(chain(&got, u).is_empty(), "unique {u} ran disarmed");
    }
}

/// An armed kernel READ (cold, device-backed) yields the read-serve chain
/// for its `unique`, with the fill's device stamps carried by the
/// request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn armed_read_yields_an_ordered_serve_chain_for_its_unique() {
    let _g = serial().await;
    let h = make([0xA3; 16], "op_trace_read").await;
    let ino =
        h.fs.create(req(2), 1, OsStr::new("r.bin"), libc::S_IFREG | 0o644, 0)
            .await
            .unwrap()
            .attr
            .ino;
    let data = pattern(BS as usize * 4, 0x33);
    h.fs.write(req(4), ino, 0, 0, bytes::Bytes::from(data.clone()), 0, 0)
        .await
        .unwrap();
    h.fs.fsync(req(6), ino, 0, false).await.unwrap();
    // Cold: purge every read-tier retention of every current block key
    // (the read_serve_phase_tests `make_cold` helper) so the read pays
    // the device.
    let path = squeezefs::keys::inode_path(ino);
    let map =
        h.fs.router
            .fetch_metadata(&path)
            .await
            .unwrap()
            .block_map
            .unwrap_or_default();
    assert!(!map.is_empty(), "fixture premise: striped");
    for key in map.values() {
        h.fs.router.cache.purge_block_key(key);
    }

    op_trace::arm_for_tests(1);
    const UNIQUE: u64 = 2_000;
    let r = op_trace::scope(UNIQUE, h.fs.read(req(UNIQUE), ino, 0, 0, BS as u32, 0))
        .await
        .unwrap();
    assert_eq!(r.data.len(), BS as usize);
    let got = op_trace::drain();
    let c = chain(&got, UNIQUE);
    assert!(!c.is_empty(), "the traced read left samples: {got:?}");
    assert!(
        c.windows(2).all(|w| w[0].mono_ns <= w[1].mono_ns),
        "chain is time-ordered: {c:?}"
    );
    for st in [Stage::ReadRouted, Stage::MetaResolved, Stage::ReadReturn] {
        assert!(has_stage(&c, st), "read chain lacks {st:?}: {c:?}");
    }
    assert!(
        has_stage(&c, Stage::DevSubmit) && has_stage(&c, Stage::DevComplete),
        "a cold read's fill stamps the device funnel: {c:?}"
    );
    assert!(stage_ns(&c, Stage::ReadRouted) <= stage_ns(&c, Stage::DevSubmit));
    assert!(stage_ns(&c, Stage::DevComplete) <= stage_ns(&c, Stage::ReadReturn));
}

/// An armed CREATE yields the meta-op chain AND the conveyor chain under
/// the same op_id: the tx carries the originating op's id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn armed_create_carries_its_unique_through_the_conveyor() {
    let _g = serial().await;
    let h = make([0xA4; 16], "op_trace_create").await;
    op_trace::arm_for_tests(1);
    const UNIQUE: u64 = 3_000;
    op_trace::scope(
        UNIQUE,
        h.fs.create(
            req(UNIQUE),
            1,
            OsStr::new("c.bin"),
            libc::S_IFREG | 0o644,
            0,
        ),
    )
    .await
    .unwrap();
    let got = op_trace::drain();
    let c = chain(&got, UNIQUE);
    assert!(!c.is_empty(), "the traced create left samples: {got:?}");
    assert!(
        c.windows(2).all(|w| w[0].mono_ns <= w[1].mono_ns),
        "chain is time-ordered: {c:?}"
    );
    for st in [
        Stage::MetaBackendStart,
        Stage::MetaEnqueue,
        Stage::PassBegin,
        Stage::JournalWritten,
        Stage::Fanout,
        Stage::MetaBackendDone,
        Stage::MetaOpReturn,
    ] {
        assert!(has_stage(&c, st), "create chain lacks {st:?}: {c:?}");
    }
    assert!(stage_ns(&c, Stage::MetaBackendStart) <= stage_ns(&c, Stage::MetaEnqueue));
    assert!(stage_ns(&c, Stage::MetaEnqueue) <= stage_ns(&c, Stage::PassBegin));
    assert!(stage_ns(&c, Stage::PassBegin) <= stage_ns(&c, Stage::JournalWritten));
    assert!(stage_ns(&c, Stage::JournalWritten) <= stage_ns(&c, Stage::Fanout));
    assert!(stage_ns(&c, Stage::Fanout) <= stage_ns(&c, Stage::MetaBackendDone));
}

// ---------------------------------------------------------------------------
// 5 — the `.trace` export
// ---------------------------------------------------------------------------

/// The export shape: `{armed, divisor, dropped, samples_total, clock,
/// stages: {id: name}, samples: [[op_id, stage, ns], …]}` sorted by
/// (op_id, ns); the payload DRAINS the rings.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_json_shape_is_pinned_and_drains() {
    let _g = serial().await;
    op_trace::arm_for_tests(1);
    // The counters are cumulative over the ring's life (rows read deltas).
    let base_total = op_trace::samples_total();
    let base_dropped = op_trace::dropped();
    let t0 = Instant::now();
    op_trace::stamp(9, Stage::Dispatch, t0 + std::time::Duration::from_nanos(5));
    op_trace::stamp(9, Stage::TransportRecv, t0);
    op_trace::stamp(8, Stage::ReplyCommit, t0);
    let json = op_trace::trace_json();
    let obj = json.as_object().expect("object");
    let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "armed",
            "clock",
            "divisor",
            "dropped",
            "samples",
            "samples_total",
            "stages"
        ]
    );
    assert_eq!(json["armed"], serde_json::Value::Bool(true));
    assert_eq!(json["divisor"].as_u64(), Some(1));
    assert_eq!(json["dropped"].as_u64(), Some(base_dropped));
    assert!(json["samples_total"].as_u64().unwrap() >= base_total + 3);
    assert_eq!(json["clock"].as_str(), Some("CLOCK_MONOTONIC_ns"));
    let stages = json["stages"].as_object().expect("stage table");
    assert_eq!(stages.len(), Stage::ALL.len());
    assert_eq!(
        stages[&(Stage::TransportRecv as u16).to_string()].as_str(),
        Some("transport_recv")
    );
    // Detached tasks of an earlier fixture may still stamp under their
    // own ids: judge OUR rows only.
    let samples: Vec<&serde_json::Value> = json["samples"]
        .as_array()
        .expect("samples array")
        .iter()
        .filter(|r| matches!(r[0].as_u64(), Some(8) | Some(9)))
        .collect();
    assert_eq!(samples.len(), 3);
    let row = |i: usize| -> (u64, u64, u64) {
        let r = samples[i].as_array().expect("triple");
        assert_eq!(r.len(), 3);
        (
            r[0].as_u64().unwrap(),
            r[1].as_u64().unwrap(),
            r[2].as_u64().unwrap(),
        )
    };
    assert_eq!(row(0).0, 8);
    assert_eq!(row(1), (9, Stage::TransportRecv as u64, row(1).2));
    assert_eq!(row(2).1, Stage::Dispatch as u64);
    assert_eq!(row(2).2 - row(1).2, 5, "ns deltas exact");
    // Drained: the next export carries none of ours but the same table.
    let again = op_trace::trace_json();
    assert!(again["samples"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| !matches!(r[0].as_u64(), Some(8) | Some(9))));
    assert!(
        again["samples_total"].as_u64().unwrap() >= base_total + 3,
        "cumulative"
    );
    assert_eq!(again["stages"].as_object().unwrap().len(), Stage::ALL.len());
    // The disarmed export is honest about it.
    op_trace::disarm();
    let off = op_trace::trace_json();
    assert_eq!(off["armed"], serde_json::Value::Bool(false));
}

/// VAL-7a: `.trace` is a lookup-minted generation inode (like `.stats`)
/// with owner-only mode 0400 and a payload frozen per generation; the
/// stats JSON carries the `op_trace_*` gauges.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_inode_is_owner_only_and_stats_carry_the_gauges() {
    let _g = serial().await;
    let h = make([0xA5; 16], "op_trace_inode").await;
    op_trace::arm_for_tests(1);
    op_trace::stamp(77, Stage::TransportRecv, Instant::now());
    let entry =
        h.fs.lookup(req(1), 1, OsStr::new(".trace"))
            .await
            .expect(".trace resolves at the root");
    assert_eq!(
        entry.attr.perm,
        squeezefs::fuse_client::VIRTUAL_INODE_MODE,
        "VAL-7a owner-only"
    );
    assert_eq!(entry.attr.uid, 1000);
    assert!(
        squeezefs::fuse_client::is_virtual_ino(entry.attr.ino),
        "a virtual generation ino"
    );
    assert_eq!(
        squeezefs::fuse_client::virtual_class(entry.attr.ino),
        Some(squeezefs::fuse_client::VirtualClass::Trace)
    );
    let opened =
        h.fs.open(req(1), entry.attr.ino, libc::O_RDONLY as u32, 0)
            .await
            .unwrap();
    let data =
        h.fs.read(
            req(1),
            entry.attr.ino,
            opened.fh,
            0,
            entry.attr.size as u32,
            0,
        )
        .await
        .unwrap();
    assert_eq!(data.data.len() as u64, entry.attr.size, "size = payload");
    let json: serde_json::Value = serde_json::from_slice(&data.data).expect("JSON payload");
    let ours: Vec<&serde_json::Value> = json["samples"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r[0].as_u64() == Some(77))
        .collect();
    assert_eq!(ours.len(), 1, "the lookup drained our stamp");
    assert_eq!(ours[0][1].as_u64(), Some(Stage::TransportRecv as u64));
    // The lookup DRAINED: a second generation is empty.
    let entry2 = h.fs.lookup(req(1), 1, OsStr::new(".trace")).await.unwrap();
    assert_ne!(
        entry2.attr.ino, entry.attr.ino,
        "fresh generation per lookup"
    );
    let opened2 =
        h.fs.open(req(1), entry2.attr.ino, libc::O_RDONLY as u32, 0)
            .await
            .unwrap();
    let data2 =
        h.fs.read(
            req(1),
            entry2.attr.ino,
            opened2.fh,
            0,
            entry2.attr.size as u32,
            0,
        )
        .await
        .unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&data2.data).unwrap();
    assert!(json2["samples"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r[0].as_u64() != Some(77)));

    // Stats gauges.
    let stats_entry = h.fs.lookup(req(1), 1, OsStr::new(".stats")).await.unwrap();
    let so =
        h.fs.open(req(1), stats_entry.attr.ino, libc::O_RDONLY as u32, 0)
            .await
            .unwrap();
    let sd =
        h.fs.read(
            req(1),
            stats_entry.attr.ino,
            so.fh,
            0,
            stats_entry.attr.size as u32,
            0,
        )
        .await
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&sd.data).unwrap();
    let m = &stats["metrics"];
    assert_eq!(m["op_trace_armed"], serde_json::Value::Bool(true));
    // Cumulative over the ring's life (a row reads deltas).
    assert_eq!(
        m["op_trace_samples"].as_u64(),
        Some(op_trace::samples_total())
    );
    assert!(m["op_trace_samples"].as_u64().unwrap() >= 1);
    assert_eq!(m["op_trace_dropped"].as_u64(), Some(op_trace::dropped()));
    assert_eq!(m["op_trace_divisor"].as_u64(), Some(1));
}

// ---------------------------------------------------------------------------
// 6 — the knob + the derived geometry
// ---------------------------------------------------------------------------

/// `SQUEEZEFS_OP_TRACE` is a registered Bool knob (ENG-10), default off.
#[test]
fn op_trace_knob_is_registered_bool_default_off() {
    let entry =
        squeezefs::env_knobs::lookup("SQUEEZEFS_OP_TRACE").expect("SQUEEZEFS_OP_TRACE registered");
    assert!(
        matches!(entry.kind, squeezefs::env_knobs::Kind::Bool),
        "Bool kind"
    );
    assert_eq!(entry.default, "off");
}

/// The derived geometry: rings scale with the thread population, the
/// pool with the R5 budget (floored at the ring's physical minimum
/// depth), and the divisor is what makes ONE drain interval of the
/// machine's op ceiling fit the pool.
#[test]
fn derived_geometry_fits_one_drain_interval() {
    let g = op_trace::derive_geometry(200 << 30, 32);
    assert!(g.ring_capacity.is_power_of_two());
    assert!(g.rings >= 16 && g.rings <= 4096);
    assert!(g.divisor >= 1);
    let per_s = 32 * op_trace::OPS_PER_CORE_PER_S * op_trace::STAGES_PER_OP;
    let sampled_per_s = per_s / u64::from(g.divisor);
    assert!(
        sampled_per_s <= (g.rings * g.ring_capacity) as u64,
        "one drain interval of sampled stamps fits the pool: {g:?}"
    );
    // A tiny budget floors the ring depth instead of shrinking to nothing.
    let small = op_trace::derive_geometry(64 << 20, 2);
    assert!(small.ring_capacity >= op_trace::RING_MIN_CAPACITY);
    assert!(small.divisor >= 1);
    // More budget ⇒ never a larger divisor (monotone).
    let big = op_trace::derive_geometry(1 << 40, 32);
    assert!(big.divisor <= g.divisor);
}
