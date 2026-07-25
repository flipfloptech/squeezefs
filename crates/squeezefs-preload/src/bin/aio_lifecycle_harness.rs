//! Full **libaio lifecycle harness** for the preload gate's passthrough
//! and served legs (2026-07-25 field-crash class: a host app must never
//! crash — or lose events — because `libsqueezefs_il.so` is loaded).
//!
//! Exercises the REAL `libaio.so.1` entry points through the dynamic
//! global scope so a preloaded shim's interposers win exactly as they
//! do in a host application (`dlopen(RTLD_GLOBAL)` + `dlsym(RTLD_DEFAULT)`
//! — linking `-laio` at build time would add a hard dependency the
//! crate must not carry; resolving via the dlopen HANDLE would bypass
//! interposition entirely and test nothing).
//!
//! Shapes covered (elbencho's `LocalWorker::aioBlockSized` skeleton):
//!
//! - **setup-first** (mode `setup-first`): `io_setup` at worker start,
//!   BEFORE any file on the target filesystem is touched — the shape
//!   the field crash reported (contexts created at worker start).
//! - **open-first** (mode `open-first`): fd exists (and, on a SqueezeFS
//!   mount, the session-establish probe has run) before the context.
//! - **mixed** (mode `mixed`): threads alternate the two.
//!
//! Every thread runs PHASES of `io_setup` → submit qd-deep
//! `IO_CMD_PWRITE`/`IO_CMD_PREAD` batches → `io_getevents` drain →
//! `io_destroy`, then re-runs `io_setup` — the kernel commonly recycles
//! the ring mapping address, which is the trigger surface of the
//! fake-vs-real `io_context_t` confusion class (a recycled value must
//! resolve to a definite owner). Asserted per event: cookie identity,
//! original-iocb identity, exact `res`, byte-exact read-back — and per
//! run: total events == total submits (the double-dispatch tripwire).
//!
//! Exit codes: 0 = pass; 2 = SKIP (no `libaio.so.1` on this box);
//! anything else = the transparency contract is broken.

use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_long};

use squeezefs_il::aio_glue::{RawIoEvent, RawIocb, IOCB_CMD_PREAD, IOCB_CMD_PWRITE};

type IoSetupFn = unsafe extern "C" fn(c_int, *mut u64) -> c_int;
type IoDestroyFn = unsafe extern "C" fn(u64) -> c_int;
type IoSubmitFn = unsafe extern "C" fn(u64, c_long, *mut *mut RawIocb) -> c_int;
type IoGetEventsFn =
    unsafe extern "C" fn(u64, c_long, c_long, *mut RawIoEvent, *mut libc::timespec) -> c_int;

#[derive(Clone, Copy)]
struct Aio {
    setup: IoSetupFn,
    destroy: IoDestroyFn,
    submit: IoSubmitFn,
    getevents: IoGetEventsFn,
}

// SAFETY: bare fn pointers into loaded shared objects; immutable.
unsafe impl Send for Aio {}
unsafe impl Sync for Aio {}

fn resolve() -> Option<Aio> {
    // SAFETY: dlopen/dlsym with NUL-terminated literals. RTLD_GLOBAL
    // publishes libaio into the global scope; RTLD_DEFAULT then resolves
    // each symbol through the full search order (exe → LD_PRELOAD shim →
    // … → libaio), so a preloaded interposer wins exactly as in a host
    // app that links libaio.
    unsafe {
        let h = libc::dlopen(
            c"libaio.so.1".as_ptr(),
            libc::RTLD_NOW | libc::RTLD_GLOBAL,
        );
        if h.is_null() {
            return None;
        }
        let sym = |name: &'static std::ffi::CStr| -> *mut c_void {
            libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr())
        };
        let setup = sym(c"io_setup");
        let destroy = sym(c"io_destroy");
        let submit = sym(c"io_submit");
        let getevents = sym(c"io_getevents");
        if setup.is_null() || destroy.is_null() || submit.is_null() || getevents.is_null() {
            return None;
        }
        Some(Aio {
            setup: std::mem::transmute::<*mut c_void, IoSetupFn>(setup),
            destroy: std::mem::transmute::<*mut c_void, IoDestroyFn>(destroy),
            submit: std::mem::transmute::<*mut c_void, IoSubmitFn>(submit),
            getevents: std::mem::transmute::<*mut c_void, IoGetEventsFn>(getevents),
        })
    }
}

const BLOCK: usize = 4096;

struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let mut ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: posix_memalign with a power-of-two alignment.
        let rc = unsafe { libc::posix_memalign(&mut ptr, BLOCK, len) };
        assert_eq!(rc, 0, "posix_memalign failed");
        Self {
            ptr: ptr as *mut u8,
            len,
        }
    }

    fn slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: owned allocation of `len` bytes.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocation from posix_memalign above.
        unsafe { libc::free(self.ptr as *mut c_void) };
    }
}

fn pattern_byte(tid: usize, phase: usize, block: usize, i: usize) -> u8 {
    ((tid * 31 + phase * 17 + block * 7 + i) & 0xFF) as u8
}

#[allow(clippy::too_many_arguments)]
fn run_phase(aio: &Aio, fd: c_int, tid: usize, phase: usize, qd: usize, write: bool) {
    let mut ctx: u64 = 0;
    // SAFETY: io_setup contract — zeroed ctx in, ring handle out.
    let rc = unsafe { (aio.setup)(qd as c_int, &mut ctx) };
    assert_eq!(rc, 0, "io_setup(qd={qd}) failed: {rc}");
    assert_ne!(ctx, 0, "io_setup must hand out a context");

    let mut bufs: Vec<AlignedBuf> = (0..qd).map(|_| AlignedBuf::new(BLOCK)).collect();
    let mut iocbs: Vec<RawIocb> = Vec::with_capacity(qd);
    for (b, buf) in bufs.iter_mut().enumerate() {
        if write {
            for (i, byte) in buf.slice_mut().iter_mut().enumerate() {
                *byte = pattern_byte(tid, phase, b, i);
            }
        } else {
            buf.slice_mut().fill(0);
        }
        iocbs.push(RawIocb {
            data: (0xC0FFEE00 + b) as u64,
            key: 0,
            aio_rw_flags: 0,
            aio_lio_opcode: if write { IOCB_CMD_PWRITE } else { IOCB_CMD_PREAD },
            aio_reqprio: 0,
            aio_fildes: fd,
            buf: buf.ptr as u64,
            nbytes: BLOCK as u64,
            offset: (b * BLOCK) as i64,
            __pad3: 0,
            flags: 0,
            resfd: 0,
        });
    }
    let mut ptrs: Vec<*mut RawIocb> = iocbs.iter_mut().map(|c| c as *mut RawIocb).collect();

    // Submit the whole batch (kernel may accept a prefix or answer
    // -EAGAIN under pressure — both kernel-legal; drive the rest).
    let mut submitted = 0usize;
    let mut eagain = 0usize;
    while submitted < qd {
        // SAFETY: io_submit contract — live iocb pointer array.
        let rc = unsafe {
            (aio.submit)(
                ctx,
                (qd - submitted) as c_long,
                ptrs[submitted..].as_mut_ptr(),
            )
        };
        if rc == -libc::EAGAIN {
            eagain += 1;
            assert!(eagain < 10_000, "io_submit EAGAIN-livelocked at {submitted}/{qd}");
            std::thread::yield_now();
            continue;
        }
        assert!(
            rc > 0,
            "io_submit failed at {submitted}/{qd}: {rc} (a loaded shim must never \
             break a batch a bare run accepts)"
        );
        submitted += rc as usize;
    }

    // Reap exactly qd events — elbencho's phase-2 loop shape
    // (min_nr=1, small event array, bounded timeout, obj/data derefs).
    let mut got = 0usize;
    let mut seen = vec![false; qd];
    let mut spins = 0usize;
    while got < qd {
        let mut evs = [RawIoEvent {
            data: 0,
            obj: 0,
            res: 0,
            res2: 0,
        }; 4];
        let mut ts = libc::timespec {
            tv_sec: 5,
            tv_nsec: 0,
        };
        // SAFETY: io_getevents contract — our event array + timespec.
        let rc = unsafe { (aio.getevents)(ctx, 1, 4, evs.as_mut_ptr(), &mut ts) };
        assert!(rc >= 0, "io_getevents failed: {rc}");
        if rc == 0 {
            spins += 1;
            assert!(spins < 60, "io_getevents starved: {got}/{qd} after 60 waits");
            continue;
        }
        for ev in &evs[..rc as usize] {
            let b = (ev.data - 0xC0FFEE00) as usize;
            assert!(b < qd, "cookie out of range: {:#x}", ev.data);
            assert!(
                !std::mem::replace(&mut seen[b], true),
                "DUPLICATE completion for iocb {b} — double-dispatch (partial \
                 interposition) corrupts the host app's accounting"
            );
            assert_eq!(
                ev.obj, ptrs[b] as u64,
                "io_event.obj must be the ORIGINAL iocb pointer"
            );
            assert_eq!(
                ev.res, BLOCK as i64,
                "full-block res expected for iocb {b}, got {}",
                ev.res
            );
            assert_eq!(ev.res2, 0, "res2 must be clean for iocb {b}");
        }
        got += rc as usize;
    }
    assert_eq!(got, qd, "total events must equal total submits");

    if !write {
        for (b, buf) in bufs.iter_mut().enumerate() {
            for (i, byte) in buf.slice_mut().iter().enumerate() {
                assert_eq!(
                    *byte,
                    pattern_byte(tid, phase, b, i),
                    "read-back divergence at block {b} byte {i}"
                );
            }
        }
    }

    // SAFETY: io_destroy contract; the next phase's io_setup commonly
    // recycles this ring address — the confusion-class trigger surface.
    let rc = unsafe { (aio.destroy)(ctx) };
    assert_eq!(rc, 0, "io_destroy failed: {rc}");
}

fn open_direct(path: &str) -> c_int {
    let cpath = std::ffi::CString::new(path).expect("path");
    // SAFETY: open(2) with a NUL-terminated path.
    let mut fd = unsafe {
        libc::open(
            cpath.as_ptr() as *const c_char,
            libc::O_RDWR | libc::O_CREAT | libc::O_DIRECT,
            0o644 as libc::c_uint,
        )
    };
    if fd < 0 {
        // Filesystems without O_DIRECT (tmpfs): buffered is fine — the
        // lifecycle under test is the aio one, not the I/O path.
        // SAFETY: as above.
        fd = unsafe {
            libc::open(
                cpath.as_ptr() as *const c_char,
                libc::O_RDWR | libc::O_CREAT,
                0o644 as libc::c_uint,
            )
        };
    }
    assert!(fd >= 0, "open({path}) failed");
    fd
}

fn worker(aio: Aio, dir: String, tid: usize, phases: usize, qd: usize, setup_first: bool) {
    for phase in 0..phases {
        let path = format!("{dir}/aio_harness.t{tid}.p{phase}");
        let (ctx_probe, fd);
        if setup_first {
            // elbencho shape: the context exists BEFORE any fd on the
            // target filesystem is touched.
            let mut probe: u64 = 0;
            // SAFETY: io_setup contract.
            let rc = unsafe { (aio.setup)(qd as c_int, &mut probe) };
            assert_eq!(rc, 0, "pre-open io_setup failed: {rc}");
            fd = open_direct(&path);
            ctx_probe = probe;
        } else {
            fd = open_direct(&path);
            ctx_probe = 0;
        }

        run_phase(&aio, fd, tid, phase, qd, true); // write phase
        run_phase(&aio, fd, tid, phase, qd, false); // read-back phase

        if setup_first {
            // SAFETY: destroying the pre-open probe context.
            let rc = unsafe { (aio.destroy)(ctx_probe) };
            assert_eq!(rc, 0, "pre-open ctx io_destroy failed: {rc}");
        }
        // SAFETY: close(2) on our fd.
        unsafe { libc::close(fd) };
        let _ = std::fs::remove_file(&path);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: {} <dir> <setup-first|open-first|mixed> [threads=4] [phases=3] [qd=16]",
            args[0]
        );
        std::process::exit(64);
    }
    let dir = args[1].clone();
    let mode = args[2].clone();
    let threads: usize = args.get(3).map_or(4, |v| v.parse().expect("threads"));
    let phases: usize = args.get(4).map_or(3, |v| v.parse().expect("phases"));
    let qd: usize = args.get(5).map_or(16, |v| v.parse().expect("qd"));

    let Some(aio) = resolve() else {
        eprintln!("SKIP: libaio.so.1 not available");
        std::process::exit(2);
    };

    let mut handles = Vec::new();
    for tid in 0..threads {
        let aio = aio;
        let dir = dir.clone();
        let setup_first = match mode.as_str() {
            "setup-first" => true,
            "open-first" => false,
            "mixed" => tid % 2 == 0,
            other => {
                eprintln!("unknown mode {other}");
                std::process::exit(64);
            }
        };
        handles.push(std::thread::spawn(move || {
            worker(aio, dir, tid, phases, qd, setup_first)
        }));
    }
    for h in handles {
        h.join().expect("worker thread must not crash");
    }
    println!("aio lifecycle OK: mode={mode} threads={threads} phases={phases} qd={qd}");
}
