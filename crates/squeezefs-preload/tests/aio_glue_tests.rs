//! v1.1 libaio interposer glue — the pure cores, red-first (OQ-1,
//! `docs/design-preload-interception.md` §12).
//!
//! Three cores sit between the `#[no_mangle]` `io_*` symbols and the
//! `aio_core` state machine, and all three are testable without a
//! mount or a real `io_context_t`:
//!
//! - **ABI mirrors** (`RawIocb` / `RawIoEvent`): `#[repr(C)]` views of
//!   *libaio.h's userspace* `struct iocb` / `struct io_event` (NOT the
//!   kernel `linux/aio_abi.h` shapes — the interposed symbols live in
//!   `libaio.so.1`, so the userspace layout is the contract). Layout is
//!   pinned by size/offset assertions: a silent field drift here is
//!   memory corruption inside a host application.
//! - **eligibility screen** (`screen_iocb`): the glue-level ladder in
//!   front of `aio_core::classify_iocb` — opcode allow-list via the
//!   binding's rights, plus the per-op screens the classifier cannot
//!   see: eventfd completion (`IOCB_FLAG_RESFD` demands a kernel-side
//!   eventfd signal the ring never delivers), per-op RWF flags (same
//!   allow-list law as `bailout::rwf_passthrough`), and the slab-fit
//!   cap (an async ring op occupies exactly one slot — no chunking in
//!   v1.1, oversize rides the kernel lane).
//! - **ctx registry**: `io_context_t` → per-context `AioCtxState`.
//!   Unknown contexts passthrough wholesale (a full registry degrades
//!   to kernel libaio, never breaks it); `io_destroy` + fresh
//!   `io_setup` may hand back the SAME ctx value, so a removed id must
//!   be re-registrable with fresh state.

use std::mem::{align_of, offset_of, size_of};

use squeezefs_il::aio_core::{AioEvent, IocbClass, KernelLane, RingLane, RingToken};
use squeezefs_il::aio_glue::{
    screen_iocb, AioCtxRegistry, RawIoEvent, RawIocb, IOCB_CMD_PREAD, IOCB_CMD_PWRITE,
    IOCB_FLAG_RESFD,
};

// Minimal lanes for driving real state through the registry's contexts
// (the scripted fakes live in aio_core_tests; here we only need "kernel
// accepts everything, ring has no slots").
struct AcceptAllKernel;
impl KernelLane for AcceptAllKernel {
    fn submit_run(&mut self, iocb_ids: &[u64]) -> isize {
        iocb_ids.len() as isize
    }
    fn getevents(&mut self, _min: usize, _max: usize, _timeout_ms: Option<u64>) -> Vec<AioEvent> {
        Vec::new()
    }
}
struct NoSlotRing;
impl RingLane for NoSlotRing {
    fn try_submit(&mut self, _iocb_id: u64) -> Option<RingToken> {
        None
    }
    fn poll(&mut self, _tok: RingToken) -> Option<i64> {
        None
    }
    fn abandon(&mut self, _tok: RingToken) {}
}

/// Push `n` kernel-lane ops through a context (observable as
/// `kernel_pending`).
fn seed_kernel_ops(st: &mut squeezefs_il::aio_core::AioCtxState, n: usize) {
    let ids: Vec<u64> = (0..n as u64).collect();
    let classes = vec![IocbClass::Kernel; n];
    st.submit_batch(
        &mut NoSlotRing,
        &mut AcceptAllKernel,
        &ids,
        &classes,
        &|id| id,
    );
    assert_eq!(st.kernel_pending(), n);
}

// ---------------------------------------------------------------------------
// ABI mirrors: libaio.h userspace layout, pinned
// ---------------------------------------------------------------------------

#[test]
fn raw_iocb_matches_libaio_userspace_layout() {
    // x86_64 / aarch64 LP64 little-endian — the only targets the shim
    // ships on. libaio.h: data / key / aio_rw_flags / lio_opcode /
    // reqprio / fildes / u.c.{buf,nbytes,offset,__pad3,flags,resfd}.
    assert_eq!(size_of::<RawIocb>(), 64, "libaio struct iocb is 64 bytes");
    assert_eq!(align_of::<RawIocb>(), 8);
    assert_eq!(offset_of!(RawIocb, data), 0);
    assert_eq!(offset_of!(RawIocb, key), 8);
    assert_eq!(offset_of!(RawIocb, aio_rw_flags), 12);
    assert_eq!(offset_of!(RawIocb, aio_lio_opcode), 16);
    assert_eq!(offset_of!(RawIocb, aio_reqprio), 18);
    assert_eq!(offset_of!(RawIocb, aio_fildes), 20);
    assert_eq!(offset_of!(RawIocb, buf), 24);
    assert_eq!(offset_of!(RawIocb, nbytes), 32);
    assert_eq!(offset_of!(RawIocb, offset), 40);
    assert_eq!(offset_of!(RawIocb, flags), 56);
    assert_eq!(offset_of!(RawIocb, resfd), 60);
}

#[test]
fn raw_io_event_matches_libaio_userspace_layout() {
    assert_eq!(
        size_of::<RawIoEvent>(),
        32,
        "libaio struct io_event is 32 bytes"
    );
    assert_eq!(align_of::<RawIoEvent>(), 8);
    assert_eq!(offset_of!(RawIoEvent, data), 0);
    assert_eq!(offset_of!(RawIoEvent, obj), 8);
    assert_eq!(offset_of!(RawIoEvent, res), 16);
    assert_eq!(offset_of!(RawIoEvent, res2), 24);
}

// ---------------------------------------------------------------------------
// eligibility screen: the glue ladder in front of classify_iocb
// ---------------------------------------------------------------------------

const SLAB: u64 = 1 << 20; // 1 MiB slot slab, the shipped default

fn iocb(opcode: u16, nbytes: u64) -> RawIocb {
    RawIocb {
        data: 0xC00C1E,
        key: 0,
        aio_rw_flags: 0,
        aio_lio_opcode: opcode,
        aio_reqprio: 0,
        aio_fildes: 7,
        buf: 0xDEAD_0000,
        nbytes,
        offset: 4096,
        __pad3: 0,
        flags: 0,
        resfd: 0,
    }
}

/// rights = None ⇒ fd not bound; Some((read_ok, write_ok)) mirrors the
/// fd-table binding.
fn screen(io: &RawIocb, rights: Option<(bool, bool)>) -> IocbClass {
    screen_iocb(io, rights, SLAB)
}

#[test]
fn eligible_pread_and_pwrite_on_bound_fds_classify_ring() {
    let r = iocb(IOCB_CMD_PREAD, 4096);
    let w = iocb(IOCB_CMD_PWRITE, 4096);
    assert_eq!(screen(&r, Some((true, true))), IocbClass::Ring);
    assert_eq!(screen(&w, Some((true, true))), IocbClass::Ring);
    // Rights are per-direction, exactly like the sync interposers.
    assert_eq!(screen(&r, Some((true, false))), IocbClass::Ring);
    assert_eq!(screen(&w, Some((false, true))), IocbClass::Ring);
}

#[test]
fn unbound_or_wrong_rights_classify_kernel() {
    let r = iocb(IOCB_CMD_PREAD, 4096);
    let w = iocb(IOCB_CMD_PWRITE, 4096);
    assert_eq!(screen(&r, None), IocbClass::Kernel);
    assert_eq!(screen(&w, None), IocbClass::Kernel);
    assert_eq!(
        screen(&r, Some((false, true))),
        IocbClass::Kernel,
        "PREAD without read rights rides the kernel lane"
    );
    assert_eq!(
        screen(&w, Some((true, false))),
        IocbClass::Kernel,
        "PWRITE without write rights rides the kernel lane"
    );
}

#[test]
fn non_data_opcodes_classify_kernel_even_on_bound_fds() {
    // libaio.h: FSYNC=2, FDSYNC=3, POLL=5, NOOP=6, PREADV=7, PWRITEV=8.
    // The allow-list is PREAD/PWRITE only — everything else (including
    // the vectored forms, v1.1 scope) is kernel-lane, and an UNKNOWN
    // future opcode must never be ring-guessed.
    for opcode in [2u16, 3, 5, 6, 7, 8, 42] {
        assert_eq!(
            screen(&iocb(opcode, 4096), Some((true, true))),
            IocbClass::Kernel,
            "opcode {opcode} must ride the kernel lane"
        );
    }
}

#[test]
fn resfd_eventfd_completion_classifies_kernel() {
    // IOCB_FLAG_RESFD contracts a kernel-side eventfd signal on
    // completion — the ring lane cannot deliver it. Silently dropping
    // it would hang epoll-driven reactors (fallback-is-correctness).
    let mut io = iocb(IOCB_CMD_PREAD, 4096);
    io.flags = IOCB_FLAG_RESFD;
    io.resfd = 9;
    assert_eq!(screen(&io, Some((true, true))), IocbClass::Kernel);
}

#[test]
fn per_op_rwf_flags_follow_the_sync_allow_list_law() {
    // Same law as bailout::rwf_passthrough: RWF_HIPRI is a semantics-
    // free hint (served); RWF_SYNC/DSYNC/APPEND/NOWAIT — and any
    // unknown future flag — demand kernel semantics.
    let mut io = iocb(IOCB_CMD_PWRITE, 4096);
    io.aio_rw_flags = libc::RWF_HIPRI as u32;
    assert_eq!(screen(&io, Some((true, true))), IocbClass::Ring);
    for rwf in [
        libc::RWF_SYNC,
        libc::RWF_DSYNC,
        libc::RWF_APPEND,
        libc::RWF_NOWAIT,
        1 << 30, // unknown future flag
    ] {
        io.aio_rw_flags = rwf as u32;
        assert_eq!(
            screen(&io, Some((true, true))),
            IocbClass::Kernel,
            "RWF {rwf:#x} must ride the kernel lane"
        );
    }
}

#[test]
fn slab_fit_cap_and_zero_length_classify_kernel() {
    // One async ring op = one slot; no chunking in v1.1. Exactly slab-
    // sized fits; one byte over rides the kernel lane. Zero-length ops
    // are a kernel-owned edge (no slot burned on a no-op).
    assert_eq!(
        screen(&iocb(IOCB_CMD_PREAD, SLAB), Some((true, true))),
        IocbClass::Ring
    );
    assert_eq!(
        screen(&iocb(IOCB_CMD_PREAD, SLAB + 1), Some((true, true))),
        IocbClass::Kernel
    );
    assert_eq!(
        screen(&iocb(IOCB_CMD_PWRITE, 0), Some((true, true))),
        IocbClass::Kernel
    );
}

// ---------------------------------------------------------------------------
// ctx registry
// ---------------------------------------------------------------------------

#[test]
fn register_lookup_remove_roundtrip() {
    let reg: AioCtxRegistry = AioCtxRegistry::new();
    assert!(reg.lookup(0x7000).is_none(), "unknown ctx must lookup None");

    assert!(reg.register(0x7000), "fresh register succeeds");
    let st = reg.lookup(0x7000).expect("registered ctx must resolve");
    seed_kernel_ops(&mut st.lock().unwrap(), 3);

    assert!(
        reg.remove(0x7000),
        "remove reports the ctx was known (destroy drains it)"
    );
    assert!(reg.lookup(0x7000).is_none(), "removed ctx must lookup None");
    assert!(!reg.remove(0x7000), "double remove is a no-op");
}

#[test]
fn duplicate_register_is_refused() {
    let reg: AioCtxRegistry = AioCtxRegistry::new();
    assert!(reg.register(0xA));
    assert!(
        !reg.register(0xA),
        "an io_context_t value is registered at most once"
    );
    assert!(reg.lookup(0xA).is_some(), "the original registration stays");
}

#[test]
fn removed_ctx_value_is_re_registrable_with_fresh_state() {
    // io_destroy + io_setup may hand back the SAME ctx value — the new
    // registration must start from zero state, not inherit the corpse.
    let reg: AioCtxRegistry = AioCtxRegistry::new();
    assert!(reg.register(0xB00));
    seed_kernel_ops(&mut reg.lookup(0xB00).unwrap().lock().unwrap(), 5);
    assert!(reg.remove(0xB00));

    assert!(reg.register(0xB00), "same value re-registers after remove");
    let st = reg.lookup(0xB00).expect("re-registered ctx resolves");
    assert_eq!(
        st.lock().unwrap().kernel_pending(),
        0,
        "fresh state — nothing inherited from the destroyed context"
    );
}

#[test]
fn full_registry_refuses_and_degrades_to_passthrough() {
    let reg: AioCtxRegistry = AioCtxRegistry::new();
    let mut registered = 0usize;
    // Fill to capacity, whatever it is (bounded by design).
    for i in 1..=4096u64 {
        if reg.register(i) {
            registered += 1;
        } else {
            break;
        }
    }
    assert!(registered >= 64, "capacity must cover real app ctx counts");
    assert!(
        !reg.register(0xFFFF),
        "beyond capacity, register refuses (io_setup still succeeds kernel-side; \
         the whole ctx passthroughs)"
    );
    // Removing one frees a slot for the next context.
    assert!(reg.remove(1));
    assert!(reg.register(0xFFFF), "freed capacity is reusable");
}

#[test]
fn lookups_race_free_across_threads() {
    // Register/lookup from concurrent threads: distinct contexts never
    // observe each other's state (libaio serializes only PER context).
    let reg: std::sync::Arc<AioCtxRegistry> = std::sync::Arc::new(AioCtxRegistry::new());
    let mut handles = Vec::new();
    for t in 0..8u64 {
        let reg = reg.clone();
        handles.push(std::thread::spawn(move || {
            let id = 0x1000 + t;
            assert!(reg.register(id));
            for i in 0..1000usize {
                let st = reg.lookup(id).expect("own ctx always resolves");
                let mut st = st.lock().unwrap();
                assert_eq!(st.kernel_pending(), i, "no cross-ctx state bleed");
                st.submit_batch(
                    &mut NoSlotRing,
                    &mut AcceptAllKernel,
                    &[i as u64],
                    &[IocbClass::Kernel],
                    &|id| id,
                );
            }
            let st = reg.lookup(id).unwrap();
            assert_eq!(st.lock().unwrap().kernel_pending(), 1000);
            assert!(reg.remove(id));
        }));
    }
    for h in handles {
        h.join().expect("no panics under concurrency");
    }
}
