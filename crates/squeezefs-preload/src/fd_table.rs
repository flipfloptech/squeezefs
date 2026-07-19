//! The shim's **fd table** (§5.4): fd → binding, lock-free everywhere.
//!
//! Shape: a two-level radix (directory of lazily-allocated segments) so
//! slots are **never moved or copied** — growth is a directory CAS, and
//! every operation is single-slot atomics. No mutex exists in this
//! module **by design**: interposers must remain async-signal-safe
//! (POSIX lists `close` as AS-safe, so apps legitimately call it from
//! signal handlers — a handler interrupting a `dup` that held a table
//! lock on the same thread would self-deadlock).
//!
//! **Refcount law (Issue-14, normative)**: a [`Binding`] is shared by
//! every fd-table entry that `dup*` propagated it to. `close`/
//! `close_range`/the creator hygiene sweep release one *ref*; the caller
//! sends the async unbind ctl message only when a release reports the
//! binding id — which happens on the **last** ref, exactly once (the
//! refcount is monotone-to-zero: [`BindingCell::acquire`] refuses to
//! resurrect a zero count, so a racing `dup` can never revive a binding
//! whose unbind was already reported).
//!
//! **Reclamation**: cells and segments are leaked by design. A data-path
//! lookup may hold a cell pointer concurrently with the releasing close;
//! freeing would demand hazard pointers/epochs for a structure whose
//! churn is one ~32-byte cell per *bound open on the mount* — the leak
//! is bounded by open churn and beats a reclamation protocol every
//! reviewer must re-verify. (Same posture as the design's
//! "old tables leaked-by-design at ~KB scale".)

use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

/// One bound fd's rights, as granted by the daemon's `BindOk` (§5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub binding_id: u64,
    pub ino: u64,
    pub read_ok: bool,
    pub write_ok: bool,
    /// Opaque session token (the interposer registry slot this binding's
    /// session lives in) — the closer routes the last-ref unbind ctl
    /// message through it.
    pub session: usize,
}

/// The shared, refcounted cell behind every fd entry that holds one
/// binding. Leaked on release (module docs).
struct BindingCell {
    binding: Binding,
    /// Live fd-entry references. 0 = released (unbind reported); a zero
    /// count is terminal — `acquire` never resurrects it.
    refs: AtomicU32,
}

impl BindingCell {
    /// Take one more ref, unless the cell already hit zero (a racing
    /// last-close won; the binding's unbind may already be on the wire).
    fn acquire(&self) -> bool {
        self.refs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n == 0 {
                    None
                } else {
                    Some(n + 1)
                }
            })
            .is_ok()
    }

    /// Drop one ref; `Some(binding)` on the last one — the caller's
    /// cue to send the async unbind ctl message, exactly once (the
    /// returned copy carries the session token to route it).
    fn release(&self) -> Option<Binding> {
        if self.refs.fetch_sub(1, Ordering::AcqRel) == 1 {
            Some(self.binding)
        } else {
            None
        }
    }
}

/// Segment geometry: 1024 fds per segment, 4096 segments ⇒ fds < 2^22
/// (beyond any realistic `RLIMIT_NOFILE`; larger fds simply never bind —
/// lookup on them is a directory-null `None`).
const SEG_BITS: usize = 10;
const SEG_SLOTS: usize = 1 << SEG_BITS;
const DIR_SLOTS: usize = 4096;

type Segment = [AtomicPtr<BindingCell>; SEG_SLOTS];

/// The fd table. One per process (the interposer glue owns a static).
pub struct FdTable {
    dir: Box<[AtomicPtr<Segment>; DIR_SLOTS]>,
}

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: all interior state is atomics; raw cell pointers reference
// leaked (never-freed) allocations — see the module reclamation note.
unsafe impl Send for FdTable {}
unsafe impl Sync for FdTable {}

impl FdTable {
    pub fn new() -> Self {
        // A directory of nulls: 32 KiB, one allocation, at init time
        // (never on an interposed call).
        let dir: Vec<AtomicPtr<Segment>> = (0..DIR_SLOTS)
            .map(|_| AtomicPtr::new(std::ptr::null_mut()))
            .collect();
        let dir: Box<[AtomicPtr<Segment>; DIR_SLOTS]> = dir
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!("built with DIR_SLOTS entries"));
        Self { dir }
    }

    /// The slot for `fd`, if it can exist without allocating.
    fn slot(&self, fd: i32) -> Option<&AtomicPtr<BindingCell>> {
        if fd < 0 {
            return None;
        }
        let fd = fd as usize;
        let (d, s) = (fd >> SEG_BITS, fd & (SEG_SLOTS - 1));
        if d >= DIR_SLOTS {
            return None;
        }
        let seg = self.dir[d].load(Ordering::Acquire);
        if seg.is_null() {
            return None;
        }
        // SAFETY: segments are write-once directory entries pointing to
        // leaked allocations — non-null implies valid forever.
        Some(unsafe { &(*seg)[s] })
    }

    /// The slot for `fd`, allocating its segment on demand (bind/dup
    /// paths only — never the data-path lookup).
    fn slot_or_grow(&self, fd: i32) -> Option<&AtomicPtr<BindingCell>> {
        if fd < 0 {
            return None;
        }
        let fdu = fd as usize;
        let d = fdu >> SEG_BITS;
        if d >= DIR_SLOTS {
            return None; // beyond table reach: caller stays passthrough
        }
        if self.dir[d].load(Ordering::Acquire).is_null() {
            let fresh: Box<Segment> =
                Box::new(std::array::from_fn(
                    |_| AtomicPtr::new(std::ptr::null_mut()),
                ));
            let raw = Box::into_raw(fresh);
            if self.dir[d]
                .compare_exchange(
                    std::ptr::null_mut(),
                    raw,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                // Lost the race: another thread installed one. Reclaim
                // ours — nothing ever observed it.
                // SAFETY: raw came from Box::into_raw above and was never
                // published (the CAS failed).
                drop(unsafe { Box::from_raw(raw) });
            }
        }
        self.slot(fd)
    }

    /// Data-path lookup: at most three loads, no allocation, no lock.
    /// A `Some` may race a concurrent close (the daemon rejects a dead
    /// binding id with EINVAL and the op falls through — §5.4.1).
    pub fn lookup(&self, fd: i32) -> Option<Binding> {
        let cell = self.slot(fd)?.load(Ordering::Acquire);
        if cell.is_null() {
            return None;
        }
        // SAFETY: cells are leaked-by-design — a loaded non-null pointer
        // is valid forever (module reclamation note).
        Some(unsafe { (*cell).binding })
    }

    /// Install a fresh binding (refs = 1) on `fd`. Returns the binding
    /// of a displaced **stale** entry whose last ref this released
    /// (raw-syscall-closed fd whose number was reused — §5.4.1).
    pub fn bind(&self, fd: i32, binding: Binding) -> Option<Binding> {
        let slot = self.slot_or_grow(fd)?;
        let cell = Box::into_raw(Box::new(BindingCell {
            binding,
            refs: AtomicU32::new(1),
        }));
        let old = slot.swap(cell, Ordering::AcqRel);
        Self::release_cell(old)
    }

    /// `dup`/`dup2`/`dup3`/`F_DUPFD*`: propagate `oldfd`'s binding to
    /// `newfd` (or clear `newfd` if `oldfd` is unbound — dup2 implicitly
    /// closes newfd either way). Returns the binding released by the
    /// displaced entry's last ref, if any.
    pub fn on_dup(&self, oldfd: i32, newfd: i32) -> Option<Binding> {
        let propagated: *mut BindingCell = match self.slot(oldfd) {
            Some(slot) => {
                let cell = slot.load(Ordering::Acquire);
                // SAFETY: leaked cell — valid forever; acquire refuses a
                // zero count (a racing last-close won).
                if !cell.is_null() && unsafe { (*cell).acquire() } {
                    cell
                } else {
                    std::ptr::null_mut()
                }
            }
            None => std::ptr::null_mut(),
        };
        let slot = if propagated.is_null() {
            // Nothing to install: only clear a stale newfd entry — and
            // don't allocate a segment just to store null.
            self.slot(newfd)?
        } else {
            match self.slot_or_grow(newfd) {
                Some(s) => s,
                None => {
                    // newfd beyond table reach: roll the acquire back.
                    // SAFETY: leaked cell, valid forever.
                    return unsafe { (*propagated).release() };
                }
            }
        };
        let old = slot.swap(propagated, Ordering::AcqRel);
        Self::release_cell(old)
    }

    /// `close(fd)`: drop the entry's ref. `Some(binding)` = last ref —
    /// send the async unbind ctl message.
    pub fn on_close(&self, fd: i32) -> Option<Binding> {
        let slot = self.slot(fd)?;
        let old = slot.swap(std::ptr::null_mut(), Ordering::AcqRel);
        Self::release_cell(old)
    }

    /// `close_range(first, last)`: sweep the inclusive range, appending
    /// every last-ref binding to `released` (one unbind each). Whole
    /// unallocated segments are skipped in O(1) — `close_range(3, ~0)`
    /// is the systemd/container idiom and must not walk millions of
    /// nulls.
    pub fn on_close_range(&self, first: i32, last: i32, released: &mut Vec<Binding>) {
        if first < 0 || last < first {
            return;
        }
        let mut fd = first as usize;
        let last = last as usize;
        while fd <= last {
            let d = fd >> SEG_BITS;
            if d >= DIR_SLOTS {
                return; // beyond table reach: nothing there was ever bound
            }
            if self.dir[d].load(Ordering::Acquire).is_null() {
                fd = (d + 1) << SEG_BITS; // skip the whole segment
                continue;
            }
            if let Some(id) = self.on_close(fd as i32) {
                released.push(id);
            }
            fd += 1;
        }
    }

    /// The §5.6.2 W3(a) mmap rule (Issue-22): release **every** entry
    /// whose binding names `ino` — the mapping's authority is per-file,
    /// not per-fd (a same-inode sibling fd left bound would recreate the
    /// lost-update hazard through ring writes racing page writeback).
    /// Matches by ino only: two bound mounts sharing an st_ino would
    /// over-unbind, which is passthrough — correct by §5.4.2.
    pub fn unbind_ino(&self, ino: u64, released: &mut Vec<Binding>) {
        for d in 0..DIR_SLOTS {
            let seg = self.dir[d].load(Ordering::Acquire);
            if seg.is_null() {
                continue;
            }
            for s in 0..SEG_SLOTS {
                // SAFETY: non-null segments are leaked allocations.
                let slot = unsafe { &(*seg)[s] };
                let cell = slot.load(Ordering::Acquire);
                if cell.is_null() {
                    continue;
                }
                // SAFETY: leaked cell — valid forever.
                if unsafe { (*cell).binding.ino } != ino {
                    continue;
                }
                // CAS, not swap: never clobber a racing rebind of this
                // fd to some other file.
                if slot
                    .compare_exchange(
                        cell,
                        std::ptr::null_mut(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    if let Some(b) = Self::release_cell(cell) {
                        released.push(b);
                    }
                }
            }
        }
    }

    /// The fd-creator hygiene sweep (§5.1, Issue-21): a creator returned
    /// `fd`; a stale entry there means its close was invisible (raw
    /// syscall / IORING_OP_CLOSE). Release it **exactly as `close`
    /// would** — refcount decrement, unbind id on last ref — never a
    /// bare clear. The common empty case is one segment load + one slot
    /// load + branch.
    pub fn sweep_stale(&self, fd: i32) -> Option<Binding> {
        self.on_close(fd)
    }

    fn release_cell(cell: *mut BindingCell) -> Option<Binding> {
        if cell.is_null() {
            return None;
        }
        // SAFETY: leaked cell — valid forever (module reclamation note).
        unsafe { (*cell).release() }
    }
}
