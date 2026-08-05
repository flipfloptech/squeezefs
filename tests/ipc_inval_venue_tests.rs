//! The W1 inval dispatch **venue pins** (D12 board item 2 — the il
//! write residual, 2026-08-05; evidence note
//! `.benchmarks/2026-08-05-fleet-parity-writes.md`).
//!
//! The found term: every size-growing sequential ring write fired the
//! production inval hook, and the hook ran `Handle::spawn` onto the
//! daemon's **multi-thread runtime's global inject queue** — the exact
//! venue the 2026-07-26 handoff-economy fix banned for ring handoffs
//! (the measured ~130 µs/op cold-firehose queueing term; see
//! `handoff_spawn`'s docs in `src/ipc_service.rs`). The spawn existed
//! only to enter async context for an await that never suspends:
//! `Notify::invalid_inode` → `ReplyTx::send` is ONE
//! `futures_channel::mpsc::unbounded_send` — synchronous, thread-safe,
//! wake-carrying. So the venue law here is stronger than the handoff
//! one: the dispatch needs **no task and no runtime at all**.
//!
//! The pins (weakening-verified, the handoff-venue-pin discipline):
//! restoring ANY spawn — global-inject (the pre-fix shape) or a lane
//! spawn — fails `..._without_an_ambient_runtime` (the pre-fix hook
//! could not even be CONSTRUCTED outside a runtime: it captured
//! `Handle::current()`), and fails the synchronous-delivery asserts
//! (the frame would not be in the channel when `try_next_frame` polls
//! it, since no executor ever runs here).
//!
//! Correctness is deliberately NOT weakened: the frame bytes are
//! asserted against the fork's ONE shared encoder
//! (`fuse3::notify::inval_inode_frame` — the generic/451 no-drift law),
//! the scope → `(off, len)` kernel mapping is pinned verbatim
//! (AttrsOnly ⇒ `(-1, 0)`, Whole ⇒ `(0, -1)` —
//! `fuse_reverse_inval_inode`'s contract), and delivery stays
//! fire-and-forget (§5.6.2 W1's bounded-staleness adjudication,
//! documented at the hook).

use arc_swap::ArcSwap;
use squeezefs::ipc_service::{make_inval_hook, InvalScope};
use std::sync::Arc;

/// The headline pin: the production hook is constructible AND delivers
/// its frame with **no tokio runtime anywhere** — no ambient runtime on
/// this thread, no executor ever polling. Pre-fix this panicked at
/// construction (`Handle::current()` outside a runtime), which is the
/// venue dependency made visible; a re-introduced spawn also fails the
/// synchronous-delivery assert because nothing here would ever poll the
/// spawned task.
#[test]
fn inval_hook_delivers_without_an_ambient_runtime() {
    let (notify, mut rx) = fuse3::notify::notify_test_channel();
    let cell = Arc::new(ArcSwap::from_pointee(Some(notify)));
    let hook = make_inval_hook(cell);

    hook(42, InvalScope::AttrsOnly);
    let frame = rx.try_next_frame().expect(
        "the inval dispatch must enqueue its notify frame SYNCHRONOUSLY in \
         the caller's context — zero spawn, zero venue (the 2026-07-26 \
         handoff-economy law; the enqueue is one unbounded_send)",
    );
    assert_eq!(
        frame,
        fuse3::notify::inval_inode_frame(42, -1, 0),
        "AttrsOnly must be the off<0 attrs-only form, byte-identical to \
         the fork's ONE shared encoder"
    );

    hook(42, InvalScope::Whole);
    let frame = rx
        .try_next_frame()
        .expect("Whole delivers synchronously too");
    assert_eq!(
        frame,
        fuse3::notify::inval_inode_frame(42, 0, -1),
        "Whole must be the (0, -1) whole-inode form"
    );
}

/// The bind/unbind arms fire the hook from ipc ctl/service threads —
/// plain OS threads, foreign to every tokio runtime (the same caller
/// shape `handoff_spawns_from_a_plain_os_thread_without_ambient_runtime`
/// pins for handoffs). The dispatch must work from there verbatim.
#[test]
fn inval_hook_fires_from_a_plain_os_thread() {
    let (notify, mut rx) = fuse3::notify::notify_test_channel();
    let cell = Arc::new(ArcSwap::from_pointee(Some(notify)));
    let hook = make_inval_hook(cell);

    std::thread::spawn(move || {
        hook(7, InvalScope::Whole);
    })
    .join()
    .expect("dispatching from a plain OS thread must not panic");

    let frame = rx
        .try_next_frame()
        .expect("the frame is already enqueued when the firing thread joined");
    assert_eq!(frame, fuse3::notify::inval_inode_frame(7, 0, -1));
}

/// Pre-mount fires are skipped by contract (no kernel cache exists yet):
/// an empty cell dispatches nothing and panics nowhere.
#[test]
fn inval_hook_skips_pre_mount_fires() {
    let cell: Arc<ArcSwap<Option<fuse3::notify::Notify>>> = Arc::new(ArcSwap::from_pointee(None));
    let hook = make_inval_hook(cell);
    hook(1, InvalScope::AttrsOnly);
    hook(1, InvalScope::Whole);
    // Nothing to receive from — the pin is "no panic, no venue touched".
}
