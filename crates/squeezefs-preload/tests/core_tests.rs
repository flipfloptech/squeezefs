//! PR L4-5 — the shim's process-local cores, red-first
//! (`docs/design-preload-interception.md` §5.4, §5.4.1, §5.1):
//!
//! - **fd table** (§5.4): lock-free, allocation-free-on-lookup, indexed by
//!   fd; **bindings refcounted across the fd-table entries that share
//!   them** (Issue-14): `dup*` propagates + increments, `close`/
//!   `close_range` decrement and report the released binding for the
//!   async unbind ctl **only on last close**; the creator hygiene sweep
//!   releases a stale entry **exactly as `close` would** (Issue-21 —
//!   never a bare pointer clear); released bindings are tombstoned, never
//!   freed (leaked-by-design like the grown tables — no ABA/UAF against
//!   racing data-path lookups).
//! - **bail-out classification** (§5.4.1): the client-side mirror of the
//!   §5.2 daemon fd screen (an optimization, never the boundary) plus the
//!   per-op RWF flag screen (`RWF_APPEND`, `RWF_NOWAIT`, unknown ⇒
//!   passthrough).
//! - **negative st_dev cache** (§5.4): fixed-size, lock-free — every
//!   subsequent open on an already-classified non-SqueezeFS filesystem
//!   is one probe.
//!
//! These are pure cores (no syscalls, no globals) so they are testable
//! and stress-able here; the interposer glue wires them to libc.

use squeezefs_il::bailout::{classify_fd, rwf_passthrough, BindRefusal};
use squeezefs_il::dev_cache::NegativeDevCache;
use squeezefs_il::fd_table::{Binding, FdTable};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn binding(id: u64) -> Binding {
    Binding {
        binding_id: id,
        ino: 100 + id,
        read_ok: true,
        write_ok: true,
        session: 0,
    }
}

// ---------------------------------------------------------------------------
// fd table: bind / lookup / dup / close refcount law
// ---------------------------------------------------------------------------

#[test]
fn bind_lookup_close_roundtrip() {
    let t = FdTable::new();
    assert!(t.lookup(3).is_none(), "unbound fd must lookup None");

    assert_eq!(t.bind(3, binding(7)), None, "fresh bind releases nothing");
    let b = t.lookup(3).expect("bound fd must resolve");
    assert_eq!((b.binding_id, b.ino), (7, 107));
    assert!(b.read_ok && b.write_ok);

    assert_eq!(
        t.on_close(3).map(|b| b.binding_id),
        Some(7),
        "sole-entry close must report the released binding for the async unbind"
    );
    assert!(t.lookup(3).is_none(), "closed fd must lookup None");
    assert!(t.on_close(3).is_none(), "double close releases nothing");
}

#[test]
fn dup_propagates_and_close_original_keeps_dup_bound() {
    // The Issue-14 shape pinned by G-L4-4's dup transparency test:
    // dup2(a,b); close(a) leaves b intercepted — unbind fires only on the
    // LAST close of the sharing set.
    let t = FdTable::new();
    t.bind(10, binding(1));
    assert_eq!(
        t.on_dup(10, 20).map(|b| b.binding_id),
        None,
        "dup onto a free fd releases nothing"
    );

    assert_eq!(
        t.on_close(10),
        None,
        "closing the original must NOT unbind — the dup still shares it"
    );
    assert!(t.lookup(10).is_none(), "closed original must lookup None");
    let b = t
        .lookup(20)
        .expect("dup must stay bound after original close");
    assert_eq!(b.binding_id, 1);

    assert_eq!(
        t.on_close(20).map(|b| b.binding_id),
        Some(1),
        "last close unbinds exactly once"
    );
    assert!(t.lookup(20).is_none());
}

#[test]
fn dup2_over_live_binding_releases_the_clobbered_one() {
    // dup2 implicitly closes newfd: a live binding there must release
    // exactly as close would, THEN the propagation lands.
    let t = FdTable::new();
    t.bind(5, binding(1));
    t.bind(6, binding(2));
    assert_eq!(
        t.on_dup(5, 6).map(|b| b.binding_id),
        Some(2),
        "dup2 over a bound fd must release the clobbered sole-ref binding"
    );
    assert_eq!(t.lookup(6).expect("6 now shares 5's binding").binding_id, 1);
    assert!(t.on_close(5).is_none());
    assert_eq!(t.on_close(6).map(|b| b.binding_id), Some(1));
}

#[test]
fn bind_over_stale_entry_releases_it_first() {
    // open() reusing a number whose close the shim never saw (raw-syscall
    // close): the fresh bind must release the stale ref exactly as close
    // would (§5.4.1 residual-hazard mitigation).
    let t = FdTable::new();
    t.bind(4, binding(1));
    assert_eq!(
        t.bind(4, binding(2)).map(|b| b.binding_id),
        Some(1),
        "rebinding a stale entry must release the old sole-ref binding"
    );
    assert_eq!(t.lookup(4).expect("new binding live").binding_id, 2);
    assert_eq!(t.on_close(4).map(|b| b.binding_id), Some(2));
}

#[test]
fn close_range_sweeps_inclusive_range() {
    let t = FdTable::new();
    t.bind(3, binding(1));
    t.bind(5, binding(2));
    t.bind(9, binding(3));
    t.on_dup(5, 100); // sharer OUTSIDE the range keeps binding 2 alive

    let mut released = Vec::new();
    t.on_close_range(3, 9, &mut released);
    let mut released: Vec<u64> = released.iter().map(|b| b.binding_id).collect();
    released.sort_unstable();
    assert_eq!(
        released,
        vec![1, 3],
        "close_range releases sole-ref bindings in [first, last] only"
    );
    assert!(t.lookup(3).is_none() && t.lookup(5).is_none() && t.lookup(9).is_none());
    assert_eq!(
        t.lookup(100)
            .expect("out-of-range sharer stays bound")
            .binding_id,
        2
    );
    assert_eq!(t.on_close(100).map(|b| b.binding_id), Some(2));
}

#[test]
fn hygiene_sweep_is_refcount_aware_composition() {
    // THE Issue-21 composition test (named in the PR L4-5 card):
    // bind → dup → raw-syscall-close one fd (invisible to the shim) →
    // socket() reuses the number (hygiene sweep releases the stale ref)
    // → close the dup → the binding unbinds EXACTLY once.
    let t = FdTable::new();
    t.bind(7, binding(9));
    t.on_dup(7, 8);

    // fd 7 closed via raw syscall — the shim saw nothing. socket()
    // returns 7; the creator hygiene sweep runs on it:
    assert_eq!(
        t.sweep_stale(7).map(|b| b.binding_id),
        None,
        "sweep releases the stale REF (not the binding — the dup still holds one)"
    );
    assert!(t.lookup(7).is_none(), "swept entry must lookup None");
    assert_eq!(
        t.lookup(8).expect("dup unaffected by the sweep").binding_id,
        9
    );

    assert_eq!(
        t.on_close(8).map(|b| b.binding_id),
        Some(9),
        "the dup's close is the LAST ref — unbind exactly once"
    );

    // Empty-entry sweep (the overwhelmingly common case) is a no-op.
    assert!(t.sweep_stale(7).is_none());
    assert!(t.sweep_stale(1234).is_none());
}

#[test]
fn table_grows_to_large_fds_and_negative_fd_is_none() {
    let t = FdTable::new();
    t.bind(10_000, binding(1));
    assert_eq!(t.lookup(10_000).expect("large fd binds").binding_id, 1);
    assert!(t.lookup(9_999).is_none());
    assert!(
        t.lookup(-1).is_none(),
        "negative fd must lookup None, never index"
    );
    assert!(
        t.lookup(i32::MAX).is_none(),
        "huge unbound fd is a cheap None"
    );
    assert_eq!(t.on_close(10_000).map(|b| b.binding_id), Some(1));
}

#[test]
fn concurrent_dup_close_stress_unbinds_exactly_once() {
    // 8 threads race dup/close/lookup cycles on one shared binding;
    // however the interleavings land, the release must be reported
    // exactly once across all reporters.
    const THREADS: usize = 8;
    const CYCLES: usize = 2_000;

    let t = Arc::new(FdTable::new());
    let releases = Arc::new(AtomicU64::new(0));
    t.bind(3, binding(42));

    let mut handles = Vec::new();
    for i in 0..THREADS {
        let t = Arc::clone(&t);
        let releases = Arc::clone(&releases);
        handles.push(std::thread::spawn(move || {
            let my_fd = 100 + i as i32;
            for _ in 0..CYCLES {
                if t.on_dup(3, my_fd).is_some() {
                    releases.fetch_add(1, Ordering::Relaxed);
                }
                let _ = t.lookup(3);
                let _ = t.lookup(my_fd);
                if t.on_close(my_fd).is_some() {
                    releases.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("no panics under stress");
    }
    // The anchor entry still holds one ref; nothing may have released yet.
    assert_eq!(
        releases.load(Ordering::Relaxed),
        0,
        "binding must never release while the anchor fd holds a ref"
    );
    assert_eq!(
        t.on_close(3).map(|b| b.binding_id),
        Some(42),
        "the anchor close is the last ref"
    );
    assert!(t.on_close(3).is_none());
}

#[test]
fn unbind_ino_walk_releases_every_sibling_in_process() {
    // The §5.6.2 W3(a) mmap rule (Issue-22): mmap on a bound fd unbinds
    // ALL in-process bindings on the SAME inode — fd_b must stop ring-
    // writing when fd_a maps the file. Distinct inodes are untouched.
    let t = FdTable::new();
    t.bind(3, binding(1)); // ino 101
    t.bind(4, binding(2)); // ino 102 (different file)
                           // A second open of ino 101 (its own binding id):
    t.bind(
        5,
        Binding {
            binding_id: 9,
            ino: 101,
            read_ok: true,
            write_ok: true,
            session: 0,
        },
    );
    t.on_dup(3, 30); // sharer of binding 1

    let mut released = Vec::new();
    let mut flushes = Vec::new();
    t.unbind_ino(101, &mut released, &mut flushes);
    assert!(
        flushes.is_empty(),
        "plain (never-armed) binds must not surface offset flushes"
    );
    let mut released: Vec<u64> = released.iter().map(|b| b.binding_id).collect();
    released.sort_unstable();
    assert_eq!(
        released,
        vec![1, 9],
        "every ino-101 binding releases exactly once (the dup ref and the entry ref collapse)"
    );
    assert!(
        t.lookup(3).is_none() && t.lookup(5).is_none() && t.lookup(30).is_none(),
        "all ino-101 entries (incl. dup sharers) must be gone"
    );
    assert_eq!(
        t.lookup(4).expect("other inode untouched").binding_id,
        2,
        "unbind_ino must never touch other inodes"
    );
    assert_eq!(t.on_close(4).map(|b| b.binding_id), Some(2));
}

// ---------------------------------------------------------------------------
// bail-out classification (§5.4.1 bind-time rows + per-op RWF screen)
// ---------------------------------------------------------------------------

#[test]
fn classify_mirrors_the_daemon_screen_rows() {
    let reg = libc::S_IFREG;
    // Accepted access modes ⇒ per-direction rights.
    assert_eq!(classify_fd(libc::O_RDWR, reg, 1), Ok((true, true)));
    assert_eq!(classify_fd(libc::O_RDONLY, reg, 1), Ok((true, false)));
    assert_eq!(classify_fd(libc::O_WRONLY, reg, 1), Ok((false, true)));

    // Refusal rows (each a §5.4.1 ladder row).
    assert_eq!(
        classify_fd(libc::O_PATH, reg, 1),
        Err(BindRefusal::Flags),
        "O_PATH is the load-bearing refusal (search-only permission)"
    );
    assert_eq!(
        classify_fd(libc::O_RDWR | libc::O_APPEND, reg, 1),
        Err(BindRefusal::Flags)
    );
    assert_eq!(
        classify_fd(libc::O_RDWR | libc::O_SYNC, reg, 1),
        Err(BindRefusal::Flags)
    );
    assert_eq!(
        classify_fd(libc::O_RDWR | libc::O_DSYNC, reg, 1),
        Err(BindRefusal::Flags)
    );
    assert_eq!(
        classify_fd(libc::O_RDWR | libc::O_TMPFILE, reg, 1),
        Err(BindRefusal::Flags)
    );
    assert_eq!(
        classify_fd(libc::O_RDWR, reg, 0),
        Err(BindRefusal::Flags),
        "st_nlink == 0 (unnamed/unlinked) refuses — passthrough serves it"
    );
    for mode in [
        libc::S_IFDIR,
        libc::S_IFIFO,
        libc::S_IFSOCK,
        libc::S_IFBLK,
        libc::S_IFCHR,
    ] {
        assert_eq!(
            classify_fd(libc::O_RDONLY, mode, 1),
            Err(BindRefusal::NotRegular),
            "non-regular st_mode {mode:o} must refuse"
        );
    }
}

#[test]
fn rwf_screen_passes_known_safe_and_refuses_the_rest() {
    assert!(!rwf_passthrough(0), "no flags ⇒ ring-eligible");
    assert!(
        !rwf_passthrough(libc::RWF_HIPRI),
        "RWF_HIPRI is served (hint only)"
    );
    assert!(
        rwf_passthrough(libc::RWF_SYNC),
        "RWF_SYNC ⇒ passthrough (per-op durable barrier — the O_SYNC bind-refusal analogy)"
    );
    assert!(
        rwf_passthrough(libc::RWF_DSYNC),
        "RWF_DSYNC ⇒ passthrough (per-op durable barrier)"
    );
    assert!(
        rwf_passthrough(libc::RWF_APPEND),
        "RWF_APPEND ⇒ passthrough (atomic size authority)"
    );
    assert!(
        rwf_passthrough(libc::RWF_NOWAIT),
        "RWF_NOWAIT ⇒ passthrough (known-but-unservable)"
    );
    assert!(
        rwf_passthrough(1 << 30),
        "UNKNOWN future flags must passthrough, never be silently dropped"
    );
}

// ---------------------------------------------------------------------------
// negative st_dev cache (§5.4 per-open classification cost)
// ---------------------------------------------------------------------------

#[test]
fn negative_dev_cache_remembers_and_never_false_positives() {
    let c = NegativeDevCache::new();
    assert!(!c.contains(11), "empty cache holds nothing");
    c.insert(11);
    c.insert(22);
    assert!(c.contains(11) && c.contains(22));
    assert!(!c.contains(33), "never-inserted dev must be a miss");
    // dev 0 is a legal st_dev value (virtual filesystems) — must be
    // representable, never confused with an empty slot.
    c.insert(0);
    assert!(c.contains(0));
}

#[test]
fn negative_dev_cache_is_bounded_evicts_gracefully() {
    let c = NegativeDevCache::new();
    // Far past any plausible capacity: inserts must stay cheap and the
    // MOST RECENT inserts must still hit (eviction may drop older ones —
    // it is a cache; a miss just re-pays one fstatfs).
    for dev in 1..=10_000u64 {
        c.insert(dev);
    }
    assert!(c.contains(10_000), "the most recent insert must be present");
    let hits = (1..=10_000u64).filter(|d| c.contains(*d)).count();
    assert!(
        hits <= 4096,
        "cache must be bounded (fixed-size), got {hits} resident entries"
    );
}

#[test]
fn negative_dev_cache_concurrent_probe_insert_stress() {
    let c = Arc::new(NegativeDevCache::new());
    let mut handles = Vec::new();
    for t in 0..8u64 {
        let c = Arc::clone(&c);
        handles.push(std::thread::spawn(move || {
            for i in 0..10_000u64 {
                let dev = t * 1_000_000 + (i % 64);
                c.insert(dev);
                // Lock-free probes must never see torn values: a hit's
                // value equality is checked inside contains().
                let _ = c.contains(dev);
            }
        }));
    }
    for h in handles {
        h.join().expect("no panics under stress");
    }
}
