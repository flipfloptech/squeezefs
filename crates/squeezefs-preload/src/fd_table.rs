//! The shim's **fd table** (§5.4): fd → binding, lock-free everywhere.
//!
//! Shape: a two-level radix (directory of lazily-allocated segments) so
//! slots are **never moved or copied** — growth is a directory CAS, and
//! every operation is single-slot atomics. No mutex guards any *table
//! operation* **by design**: interposers must remain async-signal-safe
//! (POSIX lists `close` as AS-safe, so apps legitimately call it from
//! signal handlers — a handler interrupting a `dup` that held a table
//! lock on the same thread would self-deadlock). The one mutex that
//! lives *inside* a cell (the offset-mirror lock below) is never taken
//! by bind/dup/close/sweep — only by interposer offsetful-op contexts,
//! which are already reentrancy-guarded (a signal handler entering the
//! shim over an interposed frame takes the real call, never the lock),
//! and the fork-prepare walk only ever `try_lock`s it (a CAS, no park).
//!
//! ## Offset mirror (PERF-7, `docs/pre-rc-engineering-spec.md` §9)
//!
//! Offsetful ops (`read`/`write`/`readv`/`writev`) consume the file
//! description's `f_pos`. Pre-PERF-7 the shim kept the KERNEL offset
//! authoritative at +2 `lseek64` syscalls per op (§5.4.3). The mirror
//! moves that authority into the [`BindingCell`] — the object dup'd
//! fds already share, so intra-process `dup` keeps kernel
//! shared-`f_pos` semantics **exactly** (one cell = one offset = one
//! lock). What the cell cannot model is a description shared with
//! another *process*; the normative predicate is therefore:
//!
//! > **The mirror is authoritative for a binding iff (a) it was armed
//! > at bind with a fork-epoch snapshot taken before its fd existed,
//! > (b) that snapshot still equals the table's current fork epoch,
//! > and (c) nothing has disarmed it** (the atfork-prepare /
//! > posix_spawn flush, an unbind-class demote — locks, O_APPEND,
//! > O_DIRECT toggle, mmap, poison — or an offsetful fallthrough the
//! > real call must serve).
//!
//! Anything outside the predicate — and every unbound fd — runs the
//! kernel-authoritative lseek-resync discipline, i.e. exactly the
//! pre-PERF-7 behavior. Demotes FLUSH (mirror → kernel `f_pos`) before
//! kernel authority resumes, so fork children and post-demote real
//! calls resume at the true offset. The disarm/publish pair is a
//! Dekker protocol (SeqCst fences on both sides — see [`MirrorHandle`])
//! so an op completing concurrently with a demote either publishes
//! into a still-armed mirror or writes its final offset through to the
//! kernel itself; the demote side only issues its own flush syscall
//! when it holds the cell's offset lock (single-issuer), falling back
//! to a documented fork-concurrent-with-in-flight-I/O residual that is
//! the same class the §5.4.3 exactness claim already excluded
//! (cross-process concurrent offsetful racing).
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

use std::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

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

/// The per-cell offset mirror (PERF-7 — module docs). Shared by every
/// dup sibling exactly because the cell is.
struct MirrorState {
    /// The mirrored file offset. Authoritative iff [`MirrorHandle::armed`].
    off: AtomicU64,
    /// Arm flag; cleared exactly once by the first demote (monotone —
    /// a mirror never re-arms; the fd falls back to kernel authority).
    flag: AtomicBool,
    /// Fork-epoch snapshot taken BEFORE the fd existed (the caller's
    /// duty — interpose snapshots before the real `open`). Immutable.
    epoch: u64,
    /// Offsetful-op serialization for this description: dup siblings
    /// share it, so concurrent `read()`s through dup'd fds serialize
    /// like the kernel's own `f_pos_lock` (the pre-PERF-7 per-fd
    /// stripe split them — this is strictly tighter). Interposer
    /// contexts only; never touched by table ops (module AS-safety
    /// note), and the fork walk only `try_lock`s.
    lock: Mutex<()>,
}

impl MirrorState {
    fn unarmed() -> Self {
        Self {
            off: AtomicU64::new(0),
            flag: AtomicBool::new(false),
            epoch: 0,
            lock: Mutex::new(()),
        }
    }
}

/// The shared, refcounted cell behind every fd entry that holds one
/// binding. Leaked on release (module docs).
struct BindingCell {
    binding: Binding,
    /// Live fd-entry references. 0 = released (unbind reported); a zero
    /// count is terminal — `acquire` never resurrects it.
    refs: AtomicU32,
    /// PERF-7 offset mirror (module docs).
    mirror: MirrorState,
}

/// Borrowed view of one cell's offset mirror. The `'a` is the table
/// borrow; the cell itself is leaked-by-design (module reclamation
/// note), so a handle held across a concurrent close stays valid.
pub struct MirrorHandle<'a> {
    mirror: &'a MirrorState,
    table_epoch: &'a AtomicU64,
}

impl<'a> MirrorHandle<'a> {
    /// The normative authority predicate (module docs): armed flag set
    /// AND the bind-time fork-epoch snapshot is still current.
    pub fn armed(&self) -> bool {
        self.mirror.flag.load(Ordering::Acquire)
            && self.mirror.epoch == self.table_epoch.load(Ordering::Acquire)
    }

    /// The mirrored offset. Meaningful only while [`Self::armed`].
    pub fn load(&self) -> u64 {
        self.mirror.off.load(Ordering::Acquire)
    }

    /// Plain offset store (caller holds [`Self::lock_offsets`]).
    pub fn store(&self, off: u64) {
        self.mirror.off.store(off, Ordering::Release);
    }

    /// Op-completion publish — the op side of the demote Dekker pair:
    /// store the final offset, `fence(SeqCst)`, re-check the arm.
    /// `false` = a demote raced this op (fork flush, unbind demote):
    /// the CALLER owns the kernel write-through of `off` (the demoter
    /// either skipped its flush because the op held the offset lock,
    /// or issued one that this later write-through supersedes). The
    /// fence is load-bearing: without it the store and the flag load
    /// may both pass a concurrent `disarm` unseen and the final offset
    /// would reach neither the mirror's consumer nor the kernel.
    pub fn publish(&self, off: u64) -> bool {
        self.mirror.off.store(off, Ordering::Release);
        fence(Ordering::SeqCst);
        self.armed()
    }

    /// Demote side of the Dekker pair: clear the arm exactly once and
    /// capture the latest mirrored offset for the caller's kernel
    /// flush. `None` = this cell was not authoritative (never armed,
    /// stale epoch, or already demoted) — the caller must NOT touch
    /// kernel `f_pos` (flushing a never-armed seed would REWIND an
    /// offset the kernel has legitimately advanced).
    pub fn disarm_if_current(&self) -> Option<u64> {
        if !self.armed() {
            return None;
        }
        if !self.mirror.flag.swap(false, Ordering::AcqRel) {
            return None; // another demoter won
        }
        fence(Ordering::SeqCst);
        Some(self.mirror.off.load(Ordering::Acquire))
    }

    /// The description's offsetful-op lock (dup siblings share it).
    /// `None` only on poisoning (a panicked holder) — callers treat
    /// that as "take the real call".
    pub fn lock_offsets(&self) -> Option<MutexGuard<'a, ()>> {
        self.mirror.lock.lock().ok()
    }

    fn try_lock_offsets(&self) -> Option<MutexGuard<'a, ()>> {
        self.mirror.lock.try_lock().ok()
    }
}

/// Pure seek arithmetic for the interposed `lseek`/`lseek64` mirror arm
/// (`SEEK_SET`/`SEEK_CUR` only — `SEEK_END`/`SEEK_DATA`/`SEEK_HOLE`
/// need the kernel's size/extent authority and are routed to the real
/// call by the interposer, keeping the POSIX-8 `i_size` staleness scope
/// exactly where it was). Matches the 64-bit kernel: a negative or
/// sign-wrapped result is `EINVAL` (`vfs_setpos`).
pub fn mirror_seek(cur: u64, offset: i64, whence: i32) -> Result<u64, i32> {
    match whence {
        libc::SEEK_SET => {
            if offset < 0 {
                Err(libc::EINVAL)
            } else {
                Ok(offset as u64)
            }
        }
        libc::SEEK_CUR => match (cur as i64).checked_add(offset) {
            Some(n) if n >= 0 => Ok(n as u64),
            _ => Err(libc::EINVAL),
        },
        _ => Err(libc::EINVAL),
    }
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
    /// PERF-7 fork epoch (module docs): bumped by [`Self::fork_demote_flush`]
    /// (the atfork-prepare / posix_spawn hook). Lives on the table, not
    /// a process static, so test tables cannot disarm each other.
    fork_epoch: AtomicU64,
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
        Self {
            dir,
            fork_epoch: AtomicU64::new(0),
        }
    }

    /// The current fork epoch — snapshot this BEFORE the real `open`
    /// syscall of any fd whose bind will arm a mirror (the bind-vs-fork
    /// race closure: a fork between snapshot and install stales the
    /// snapshot, so the binding never arms — conservative-correct).
    pub fn fork_epoch(&self) -> u64 {
        self.fork_epoch.load(Ordering::SeqCst)
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

    /// Offsetful-path lookup: the binding plus its offset-mirror handle
    /// (PERF-7). Same cost class as [`Self::lookup`]; the handle stays
    /// valid across a racing close (leaked cell).
    pub fn lookup_with_mirror(&self, fd: i32) -> Option<(Binding, MirrorHandle<'_>)> {
        let cell = self.slot(fd)?.load(Ordering::Acquire);
        if cell.is_null() {
            return None;
        }
        // SAFETY: leaked cell — valid forever (module reclamation note).
        let cell = unsafe { &*cell };
        Some((
            cell.binding,
            MirrorHandle {
                mirror: &cell.mirror,
                table_epoch: &self.fork_epoch,
            },
        ))
    }

    /// The fork/spawn demote walk (PERF-7 — the atfork-**prepare** and
    /// `posix_spawn` hook): bump the fork epoch, then flush every cell
    /// that was armed under the *previous* epoch — `f(fd, off)` once
    /// per CELL (dup siblings share one) with its latest mirrored
    /// offset; the caller writes it to kernel `f_pos` (raw `SYS_lseek`)
    /// so the child inherits, and the demoted parent resyncs from, the
    /// true offset. Never-armed and stale-epoch cells are skipped
    /// entirely (flushing them would rewind kernel-authoritative
    /// offsets). AS-safe by construction: atomics, a bounded
    /// `try_lock` spin (never a park), and whatever `f` does (the
    /// production caller issues raw syscalls only).
    ///
    /// Single-issuer discipline: the flush for a cell is emitted under
    /// its offset lock when it can be had; if an in-flight op holds the
    /// lock past the bounded spin, the flush is emitted anyway with a
    /// freshly-loaded offset while the op's own `publish` (which now
    /// observes the cleared arm) write-throughs its final offset — two
    /// racing `SEEK_SET`s whose adverse ordering is the documented
    /// fork-concurrent-with-in-flight-I/O residual (module docs).
    pub fn fork_demote_flush(&self, mut f: impl FnMut(i32, u64)) {
        let prev = self.fork_epoch.fetch_add(1, Ordering::SeqCst);
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
                let cell = unsafe { &*cell };
                if cell.mirror.epoch != prev {
                    // Stale (never armed since an earlier fork) or
                    // post-bump snapshot (its open began after this
                    // bump ⇒ the fd cannot be in the child): both keep
                    // their state — no flush, no clear.
                    continue;
                }
                if !cell.mirror.flag.swap(false, Ordering::AcqRel) {
                    continue; // was not armed (or already demoted)
                }
                fence(Ordering::SeqCst);
                let h = MirrorHandle {
                    mirror: &cell.mirror,
                    table_epoch: &self.fork_epoch,
                };
                // Bounded try-lock spin: an in-flight offsetful op
                // holds this only for its serve window.
                let mut guard = h.try_lock_offsets();
                for _ in 0..1024 {
                    if guard.is_some() {
                        break;
                    }
                    std::hint::spin_loop();
                    guard = h.try_lock_offsets();
                }
                let fd = ((d << SEG_BITS) | s) as i32;
                f(fd, cell.mirror.off.load(Ordering::Acquire));
                drop(guard);
            }
        }
    }

    /// Install a fresh binding (refs = 1) on `fd`, **without** arming
    /// the offset mirror — offsetful ops on it run the kernel
    /// lseek-resync discipline. Returns the binding of a displaced
    /// **stale** entry whose last ref this released (raw-syscall-closed
    /// fd whose number was reused — §5.4.1).
    pub fn bind(&self, fd: i32, binding: Binding) -> Option<Binding> {
        self.install(fd, binding, MirrorState::unarmed())
    }

    /// Install a fresh binding with an **armed** offset mirror
    /// (PERF-7): `seed_off` is the fd's current kernel offset (freshly
    /// read at classify time), `epoch` the fork-epoch snapshot taken
    /// BEFORE the fd's `open` syscall. A stale snapshot installs the
    /// mirror but it never reads armed (module predicate) — the
    /// fallthrough is silent, structural, and costs nothing.
    pub fn bind_with_mirror(
        &self,
        fd: i32,
        binding: Binding,
        seed_off: u64,
        epoch: u64,
    ) -> Option<Binding> {
        self.install(
            fd,
            binding,
            MirrorState {
                off: AtomicU64::new(seed_off),
                flag: AtomicBool::new(true),
                epoch,
                lock: Mutex::new(()),
            },
        )
    }

    fn install(&self, fd: i32, binding: Binding, mirror: MirrorState) -> Option<Binding> {
        let slot = self.slot_or_grow(fd)?;
        let cell = Box::into_raw(Box::new(BindingCell {
            binding,
            refs: AtomicU32::new(1),
            mirror,
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
    ///
    /// PERF-7: the unbound fds live on kernel-served, so every ARMED
    /// cell surfaces `(fd, latest mirrored offset)` in `flushes` —
    /// exactly once per cell — and the caller restores kernel `f_pos`
    /// before any real call consumes it.
    pub fn unbind_ino(&self, ino: u64, released: &mut Vec<Binding>, flushes: &mut Vec<(i32, u64)>) {
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
                // Demote BEFORE the entry clears: after the CAS the
                // cell is unreachable and the mirrored offset would be
                // stranded. disarm_if_current is once-per-cell, so dup
                // siblings contribute a single flush.
                // SAFETY: leaked cell — valid forever.
                let h = MirrorHandle {
                    mirror: unsafe { &(*cell).mirror },
                    table_epoch: &self.fork_epoch,
                };
                if let Some(off) = h.disarm_if_current() {
                    flushes.push(((d << SEG_BITS | s) as i32, off));
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
