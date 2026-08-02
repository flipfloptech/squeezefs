//! PERF-7 — the **bound-fd offset mirror** contracts, red-first
//! (`docs/pre-rc-engineering-spec.md` §9 PERF-7; execution-plan D7
//! Track 2): every offsetful shim op paid two `lseek64` syscalls
//! against a zero-syscall design. The mirror lives in the SHARED
//! `BindingCell` — the one place dup'd fds already share — so
//! intra-process dup keeps kernel shared-`f_pos` semantics exactly,
//! and everything the shim cannot prove unshared demotes back to the
//! kernel-authoritative lseek-resync discipline (§5.4.3 as amended).
//!
//! The predicate under test (normative, documented in `fd_table.rs`):
//! a binding's mirror is authoritative **iff** (a) it was armed at
//! bind with a fork-epoch snapshot taken before its fd existed, (b)
//! that snapshot still equals the table's current fork epoch, and (c)
//! nothing disarmed it (fork/spawn flush, unbind-class demotes,
//! poison). Everything else — and every unbound fd — is the real
//! call against the kernel's `f_pos`, exactly as before PERF-7.

use squeezefs_il::bailout::{classify_fd, BindRefusal};
use squeezefs_il::fd_table::{mirror_seek, Binding, FdTable};

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
// arming: plain bind stays kernel-authoritative; bind_with_mirror arms
// ---------------------------------------------------------------------------

#[test]
fn plain_bind_leaves_mirror_unarmed() {
    // The honest-subset default: a binding installed without a mirror
    // seed (no epoch proof) serves offsetful ops via the kernel
    // lseek-resync discipline — armed() must be false.
    let t = FdTable::new();
    t.bind(3, binding(1));
    let (_b, m) = t.lookup_with_mirror(3).expect("bound fd resolves");
    assert!(
        !m.armed(),
        "a plain bind must NOT arm the mirror (kernel f_pos stays authoritative)"
    );
}

#[test]
fn bind_with_mirror_arms_and_tracks_offsets() {
    let t = FdTable::new();
    let ep = t.fork_epoch();
    t.bind_with_mirror(3, binding(1), 0, ep);
    let (b, m) = t.lookup_with_mirror(3).expect("bound fd resolves");
    assert_eq!(b.binding_id, 1);
    assert!(m.armed(), "current-epoch bind_with_mirror must arm");
    assert_eq!(m.load(), 0, "mirror starts at the seed offset");
    m.store(4096);
    assert_eq!(m.load(), 4096);
    assert!(
        m.publish(8192),
        "publish while armed reports no write-through needed"
    );
    assert_eq!(m.load(), 8192);
}

#[test]
fn stale_epoch_bind_never_arms() {
    // The bind-vs-fork race closure: the epoch snapshot is taken BEFORE
    // the fd exists; if a fork intervened, the snapshot is stale and
    // the binding must serve via the kernel path forever.
    let t = FdTable::new();
    let stale = t.fork_epoch();
    t.fork_demote_flush(|_, _| {}); // bumps the epoch
    t.bind_with_mirror(3, binding(1), 0, stale);
    let (_b, m) = t.lookup_with_mirror(3).expect("bound fd resolves");
    assert!(
        !m.armed(),
        "a stale-epoch snapshot must never arm (fd may be shared with a fork child)"
    );
}

// ---------------------------------------------------------------------------
// dup sharing: the mirror lives where dup'd fds already share state
// ---------------------------------------------------------------------------

#[test]
fn dup_shares_one_mirror_across_fd_numbers() {
    // Kernel semantics: dup'd fds share ONE file-description offset.
    // The mirror must live in the shared BindingCell so an advance via
    // either fd is visible via the other — including across segment
    // boundaries (fd 3 and fd 3000 live in different table segments).
    let t = FdTable::new();
    let ep = t.fork_epoch();
    t.bind_with_mirror(3, binding(1), 0, ep);
    t.on_dup(3, 3000);

    let (_b, m_orig) = t.lookup_with_mirror(3).expect("original resolves");
    let (_b, m_dup) = t.lookup_with_mirror(3000).expect("dup resolves");
    assert!(m_dup.armed(), "dup propagates the armed mirror");

    m_orig.store(12288);
    assert_eq!(
        m_dup.load(),
        12288,
        "offset advance via the original must be visible via the dup (one cell)"
    );
    m_dup.store(16384);
    assert_eq!(m_orig.load(), 16384, "and symmetrically");
}

#[test]
fn dup_serialization_uses_one_lock() {
    // Offsetful ops on dup siblings must serialize on the SAME lock
    // (the shared cell's), not per-fd stripes: N threads × M advances
    // of `len` through either fd must land exactly N*M*len.
    let t = Arc::new(FdTable::new());
    let ep = t.fork_epoch();
    t.bind_with_mirror(5, binding(1), 0, ep);
    t.on_dup(5, 2077); // different segment/stripe than 5

    const THREADS: usize = 8;
    const ITERS: u64 = 2000;
    const LEN: u64 = 512;
    let mut handles = Vec::new();
    for i in 0..THREADS {
        let t = Arc::clone(&t);
        handles.push(std::thread::spawn(move || {
            let fd = if i % 2 == 0 { 5 } else { 2077 };
            for _ in 0..ITERS {
                let (_b, m) = t.lookup_with_mirror(fd).expect("bound");
                let _g = m.lock_offsets().expect("offset lock");
                let cur = m.load();
                m.store(cur + LEN);
            }
        }));
    }
    for h in handles {
        h.join().expect("no panics");
    }
    let (_b, m) = t.lookup_with_mirror(5).expect("bound");
    assert_eq!(
        m.load(),
        THREADS as u64 * ITERS * LEN,
        "advances through dup siblings must fully serialize (shared cell lock)"
    );
}

// ---------------------------------------------------------------------------
// fork inheritance: bump + flush-once + demote (the prepare handler core)
// ---------------------------------------------------------------------------

#[test]
fn fork_demote_flushes_each_armed_cell_exactly_once() {
    let t = FdTable::new();
    let ep = t.fork_epoch();
    t.bind_with_mirror(3, binding(1), 0, ep);
    t.on_dup(3, 300); // dup pair shares ONE cell → one flush
    t.bind_with_mirror(7, binding(2), 0, ep);
    // A plain (never-armed) bind must not be flushed at all.
    t.bind(9, binding(3));

    let (_b, m3) = t.lookup_with_mirror(3).expect("bound");
    m3.store(4096);
    let (_b, m7) = t.lookup_with_mirror(7).expect("bound");
    m7.store(8192);

    let mut flushed: Vec<(i32, u64)> = Vec::new();
    t.fork_demote_flush(|fd, off| flushed.push((fd, off)));
    flushed.sort_unstable();
    assert_eq!(
        flushed,
        vec![(3, 4096), (7, 8192)],
        "one flush per armed CELL (dup pair collapses; the plain bind is silent), \
         carrying the latest mirrored offset"
    );

    for fd in [3, 300, 7] {
        let (_b, m) = t.lookup_with_mirror(fd).expect("still bound");
        assert!(!m.armed(), "fd {fd} must be demoted after the fork flush");
    }

    // Second fork: nothing armed remains — zero flushes.
    let mut again = 0usize;
    t.fork_demote_flush(|_, _| again += 1);
    assert_eq!(again, 0, "a demoted table flushes nothing on the next fork");
}

#[test]
fn fork_demote_never_rewinds_a_kernel_authoritative_fd() {
    // The rewind hazard: a cell whose epoch is ALREADY stale was never
    // armed — its kernel f_pos is authoritative and may have advanced
    // via real ops the mirror never saw. A later fork's flush walk must
    // NOT write its stale seed back to the kernel.
    let t = FdTable::new();
    let stale = t.fork_epoch();
    t.fork_demote_flush(|_, _| {}); // fork #1: epoch moves past `stale`
    t.bind_with_mirror(3, binding(1), 5, stale); // never armed (stale snapshot)

    let mut flushed: Vec<(i32, u64)> = Vec::new();
    t.fork_demote_flush(|fd, off| flushed.push((fd, off))); // fork #2
    assert!(
        flushed.is_empty(),
        "a never-armed (stale-epoch) cell must never be flushed — flushing would \
         rewind the kernel's authoritative f_pos to the bind-time seed"
    );
}

// ---------------------------------------------------------------------------
// demote sites: disarm-once with the latest offset (fcntl/mmap/poison arms)
// ---------------------------------------------------------------------------

#[test]
fn disarm_if_current_captures_latest_offset_exactly_once() {
    let t = FdTable::new();
    let ep = t.fork_epoch();
    t.bind_with_mirror(3, binding(1), 0, ep);
    let (_b, m) = t.lookup_with_mirror(3).expect("bound");
    m.store(65536);

    assert_eq!(
        m.disarm_if_current(),
        Some(65536),
        "first demote captures the latest mirrored offset for the kernel flush"
    );
    assert!(!m.armed());
    assert_eq!(
        m.disarm_if_current(),
        None,
        "demote is once — a second call must not re-flush"
    );
}

#[test]
fn publish_after_demote_reports_write_through_needed() {
    // The Dekker pair (store → SeqCst fence → armed recheck): an op
    // completing concurrently with a demote must learn it lost the
    // arm and write its final offset through to the kernel itself.
    let t = FdTable::new();
    let ep = t.fork_epoch();
    t.bind_with_mirror(3, binding(1), 0, ep);
    let (_b, m) = t.lookup_with_mirror(3).expect("bound");
    assert!(m.publish(4096), "armed publish needs no write-through");
    assert_eq!(m.disarm_if_current(), Some(4096));
    assert!(
        !m.publish(8192),
        "a publish that observes the demote must report write-through-needed"
    );
}

#[test]
fn unbind_ino_reports_armed_flushes_per_cell() {
    // The mmap W3(a) walk unbinds every same-ino binding; each ARMED
    // cell must surface (fd, latest offset) exactly once so the caller
    // can restore kernel f_pos before the fds go kernel-served.
    let t = FdTable::new();
    let ep = t.fork_epoch();
    t.bind_with_mirror(3, binding(1), 0, ep); // ino 101, armed
    t.on_dup(3, 30); // sharer — same cell, no extra flush
    t.bind(
        5,
        Binding {
            binding_id: 9,
            ino: 101,
            read_ok: true,
            write_ok: true,
            session: 0,
        },
    ); // ino 101, never armed — released but not flushed
    t.bind_with_mirror(4, binding(2), 0, ep); // ino 102 — untouched

    let (_b, m) = t.lookup_with_mirror(3).expect("bound");
    m.store(20480);

    let mut released = Vec::new();
    let mut flushes: Vec<(i32, u64)> = Vec::new();
    t.unbind_ino(101, &mut released, &mut flushes);

    let mut ids: Vec<u64> = released.iter().map(|b| b.binding_id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 9], "release semantics unchanged");
    assert_eq!(
        flushes,
        vec![(3, 20480)],
        "exactly the armed cell flushes, once, with the latest offset"
    );
    let (_b, m4) = t.lookup_with_mirror(4).expect("other ino untouched");
    assert!(m4.armed(), "unrelated inodes keep their arm");
}

// ---------------------------------------------------------------------------
// seek arithmetic core (the interposer's SEEK_SET/SEEK_CUR arm)
// ---------------------------------------------------------------------------

#[test]
fn mirror_seek_matches_the_kernel_arithmetic() {
    // SEEK_SET/SEEK_CUR are pure arithmetic over the mirror; SEEK_END/
    // SEEK_DATA/SEEK_HOLE are routed to the kernel by the interposer
    // (same i_size source as before PERF-7 — POSIX-8 scope unchanged)
    // and must be refused here.
    assert_eq!(mirror_seek(0, 4096, libc::SEEK_SET), Ok(4096));
    assert_eq!(mirror_seek(9999, 0, libc::SEEK_SET), Ok(0));
    assert_eq!(mirror_seek(4096, 4096, libc::SEEK_CUR), Ok(8192));
    assert_eq!(mirror_seek(4096, -4096, libc::SEEK_CUR), Ok(0));
    assert_eq!(mirror_seek(4096, 0, libc::SEEK_CUR), Ok(4096), "ftell idiom");

    // Negative results and signed wraps answer EINVAL — the 64-bit
    // kernel's vfs_setpos verdict for both.
    assert_eq!(mirror_seek(0, -1, libc::SEEK_SET), Err(libc::EINVAL));
    assert_eq!(mirror_seek(4095, -4096, libc::SEEK_CUR), Err(libc::EINVAL));
    assert_eq!(
        mirror_seek(i64::MAX as u64, 1, libc::SEEK_CUR),
        Err(libc::EINVAL)
    );

    // Kernel-routed whences never reach the arithmetic core.
    assert_eq!(mirror_seek(0, 0, libc::SEEK_END), Err(libc::EINVAL));
}

// ---------------------------------------------------------------------------
// O_APPEND refusal unchanged (PERF-7 must not widen the bind screen)
// ---------------------------------------------------------------------------

#[test]
fn o_append_refusal_is_unchanged() {
    // The mirror never needs append semantics BECAUSE this screen holds:
    // O_APPEND fds never bind (bind-time), and F_SETFL adding O_APPEND
    // unbinds (fcntl arm). PERF-7 must not touch either.
    assert_eq!(
        classify_fd(libc::O_RDWR | libc::O_APPEND, libc::S_IFREG, 1),
        Err(BindRefusal::Flags),
        "O_APPEND must stay refused at bind time"
    );
    assert_eq!(
        classify_fd(libc::O_WRONLY | libc::O_APPEND, libc::S_IFREG, 1),
        Err(BindRefusal::Flags)
    );
}

// ---------------------------------------------------------------------------
// unbound-fd passthrough: the interposed lseek must be verbatim
// ---------------------------------------------------------------------------

/// With `--features interposers` (the sanctioned test profile) this
/// test binary carries the strong `lseek`/`lseek64` symbols itself, so
/// `libc::lseek*` below exercises OUR interposer's unbound path end to
/// end; without the feature it pins real-libc semantics — either way
/// the observable contract is identical (fallback-is-correctness).
#[test]
fn unbound_fd_lseek_is_verbatim_passthrough() {
    let path = std::env::temp_dir().join(format!("sqz_perf7_lseek_{}", std::process::id()));
    std::fs::write(&path, b"0123456789abcdef").expect("fixture write");
    let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("cstring");
    // SAFETY: plain open(2) of the fixture file.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY) };
    assert!(fd >= 0, "fixture open");

    // SAFETY: plain lseek/read on our own fd.
    unsafe {
        assert_eq!(libc::lseek(fd, 4, libc::SEEK_SET), 4);
        assert_eq!(libc::lseek(fd, 0, libc::SEEK_CUR), 4);
        assert_eq!(libc::lseek64(fd, 2, libc::SEEK_CUR), 6);
        assert_eq!(libc::lseek(fd, 0, libc::SEEK_END), 16, "size via kernel");
        let mut buf = [0u8; 4];
        assert_eq!(libc::lseek(fd, 10, libc::SEEK_SET), 10);
        assert_eq!(libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 4), 4);
        assert_eq!(&buf, b"abcd", "reads continue from the seeked offset");
        // Negative target: kernel EINVAL, offset unchanged.
        *libc::__errno_location() = 0;
        assert_eq!(libc::lseek(fd, -1, libc::SEEK_SET), -1);
        assert_eq!(*libc::__errno_location(), libc::EINVAL);
        assert_eq!(libc::lseek(fd, 0, libc::SEEK_CUR), 14);
        libc::close(fd);
    }
    let _ = std::fs::remove_file(&path);
}
