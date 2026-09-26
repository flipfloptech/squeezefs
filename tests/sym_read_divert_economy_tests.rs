//! The ARMED writer's read-verb allocation economy (symmetric PR 13f,
//! review round 1, Issue 1).
//!
//! `KvMetaBackend::token_serve` — the read divert PR 5 / PR 12b put ahead
//! of every read verb (`find_dentry` / `getattr` / `readdir_page` /
//! `getxattr` / `listxattr`) — boxes its serve body so the verbs' futures
//! carry a pointer, not 1.9 KiB of state. The box must be reached ONLY by
//! a read a plane will serve: on an armed solo writer (the PR-14 flip
//! binary's default posture — bit 17 stamped by `format`, the knob on)
//! every read of an OWN object, and on a `SQUEEZEFS_SYMMETRIC_META=0`
//! forest every read, answers "read locally" from the gate's bits, and
//! that verdict is taken SYNC before the box (`writer_reads_locally`). The
//! first build boxed first and decided inside — one heap allocation per
//! read verb (`getattr` +1, `lookup` +2: its `find_dentry` and its
//! `getattr` each divert) on exactly the path the box's gate-1 bracket
//! runs: 8,800 → 7,600 allocations over this suite's 400 rounds.
//!
//! The pin is the forest read verb's allocation EXCESS over a flat volume
//! of the same shape, per round of `getattr` + `lookup`: **9** — the
//! forest codec's own (PR 1: the kind-routed lookups frame a forest key
//! per record access; `SQZ_ALLOC_TRACE=1 … -- --nocapture` prints the
//! deduped site table), identical on the armed writer and the `=0`
//! forest, and NOTHING from the divert. Counted on the CALLING thread
//! (the KV read is RAM-resident and runs inline on it; the armed plane's
//! cadence tasks run on their own lanes and are excluded by construction).

mod common;

use common::sym::{format_flat_member, format_stamped_member, open_under, shutdown, Knobs, SEAM};
use squeezefs::meta_backend::{Metadata, RoutedMetaBackend};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

thread_local! {
    static THREAD_ALLOCS: Cell<u64> = const { Cell::new(0) };
    static IN_TRACE: Cell<bool> = const { Cell::new(false) };
}

static TRACE: AtomicBool = AtomicBool::new(false);
static TRACE_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct CountingAlloc;

impl CountingAlloc {
    fn record(&self, layout: Layout) {
        THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
        if TRACE.load(Ordering::Relaxed) {
            IN_TRACE.with(|flag| {
                if !flag.get() {
                    flag.set(true);
                    let bt = std::backtrace::Backtrace::force_capture();
                    if let Ok(mut log) = TRACE_LOG.lock() {
                        log.push(format!("[{} B]\n{bt}", layout.size()));
                    }
                    flag.set(false);
                }
            });
        }
    }
}

// SAFETY: delegates verbatim to `System`; the side effects are a
// thread-local counter and a recursion-guarded trace hook.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.record(layout);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.record(layout);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn thread_allocs() -> u64 {
    THREAD_ALLOCS.with(Cell::get)
}

fn trace_enabled() -> bool {
    std::env::var("SQZ_ALLOC_TRACE").as_deref() == Ok("1")
}

fn print_site_table(label: &str) {
    let mut log = TRACE_LOG.lock().expect("trace log mutex");
    let mut sites: HashMap<String, u64> = HashMap::new();
    for entry in log.iter() {
        let key: String = entry
            .lines()
            .filter(|l| {
                l.contains("squeezefs")
                    && !l.contains("sym_read_divert_economy_tests")
                    && !l.contains("CountingAlloc")
            })
            .take(5)
            .map(|l| l.trim().to_string())
            .collect::<Vec<_>>()
            .join("\n    ");
        *sites.entry(key).or_default() += 1;
    }
    log.clear();
    let mut rows: Vec<_> = sites.into_iter().collect();
    rows.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("--- {label}: read-verb alloc sites (deduped, most frequent first) ---");
    for (site, n) in rows.iter().take(16) {
        println!("[{n}]\n    {site}\n");
    }
}

const OPS: u64 = 400;

/// One file under the root, read warm: `getattr(ino)` + `lookup(1, name)`
/// × `OPS` on the calling thread; the allocation count of the window.
async fn read_window(routed: &Arc<RoutedMetaBackend>, label: &str) -> u64 {
    let ino = routed
        .create(1, "own", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create")
        .ino;
    // Warm: the record's leaf resident, the DLM stripes touched.
    for _ in 0..16 {
        routed.getattr(ino).await.expect("getattr");
        routed.lookup(1, "own").await.expect("lookup");
    }
    if trace_enabled() {
        TRACE.store(true, Ordering::SeqCst);
        for _ in 0..4 {
            routed.getattr(ino).await.expect("getattr");
            routed.lookup(1, "own").await.expect("lookup");
        }
        TRACE.store(false, Ordering::SeqCst);
        print_site_table(label);
    }
    let a0 = thread_allocs();
    for _ in 0..OPS {
        routed.getattr(ino).await.expect("getattr");
        routed.lookup(1, "own").await.expect("lookup");
    }
    thread_allocs() - a0
}

/// **The forest read verbs' allocation excess over flat, per round of
/// `getattr` + `lookup`** — the forest codec's 9 (PR 1's kind-routed key
/// framing, stated, not this rung's). The divert adds none: the first
/// build read 12 (one box per `token_serve` call — `getattr` 1, `lookup`
/// 2) on the armed writer's own object.
const FOREST_READ_EXCESS_PER_ROUND: u64 = 9;

/// **An armed solo writer's own-object read verbs allocate exactly the
/// forest codec's excess over a flat volume and nothing for the divert**,
/// whose "read locally" verdict is sync and whose box is never reached
/// for them. (PR 14 retired the `=0` forest arm this pin also read: that
/// open is the door's refusal now.)
#[tokio::test(flavor = "current_thread")]
async fn an_armed_writers_own_object_read_verbs_pay_nothing_for_the_divert() {
    let _g = SEAM.lock().await;
    let dir = tempfile::tempdir().unwrap();

    let armed = {
        let uris = vec![format_stamped_member(dir.path(), "armed").await];
        let routed = open_under(&uris, &Knobs::armed()).await;
        assert!(
            routed.volumes[0].slot_lease_armed(),
            "the fixture must be the ARMED solo writer"
        );
        let n = read_window(&routed, "armed solo writer, own object").await;
        shutdown(&routed).await;
        n
    };
    let flat = {
        let uris = vec![format_flat_member(dir.path(), "flat").await];
        let routed = open_under(&uris, &Knobs::unarmed()).await;
        let n = read_window(&routed, "flat").await;
        shutdown(&routed).await;
        n
    };
    println!(
        "read-verb allocations over {OPS} × (getattr + lookup): armed own-object {armed}, \
         flat {flat} — forest excess per round: {}",
        (armed.saturating_sub(flat)) as f64 / OPS as f64,
    );
    // The window is 2 × OPS verbs; a whole allocation per verb reads as
    // +2 × OPS. The slack is one stray housekeeping allocation per 100
    // verbs, never a per-verb term.
    let slack = OPS / 50;
    let ceiling = flat + FOREST_READ_EXCESS_PER_ROUND * OPS + slack;
    assert!(
        armed <= ceiling,
        "the armed writer's own-object read verbs allocate {armed} over {OPS} rounds against \
         a ceiling of {ceiling} (flat {flat} + the forest codec's {FOREST_READ_EXCESS_PER_ROUND} \
         per round) — the divert allocates per verb again (the sync `writer_reads_locally` \
         verdict must run BEFORE `token_serve`'s box)"
    );
}
