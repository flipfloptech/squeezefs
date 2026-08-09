//! Placed-sever **assemblies** (shim-parity campaign, 2026-07-28) — the
//! 1-copy ring write path.
//!
//! The ring write path paid TWO userspace copies where the kernel path
//! pays one: arena → severed buffer (§5.5.2 severance at dequeue), then
//! severed buffer → `ActiveBlockBuf` (the merge). This module makes the
//! severed DESTINATION be the block's future backing: whole-block-stream
//! chunks of one `(ino, block)` sever into one shared **assembly**
//! ([`crate::cache::active_block::SharedBlock`]), the first merging
//! handler ADOPTS it as the overlay backing, and every sibling merge is a
//! coverage record with the copy elided (pointer proof) — one copy per
//! byte end to end, kernel parity.
//!
//! Custody laws (all inherited, none weakened):
//!
//! - **§5.5.2 severance at dequeue** — the arena is still read exactly
//!   once, synchronously on the service thread, before the handoff can
//!   park; the destination is private until adoption and claim-exclusive
//!   after ([`crate::placed_core::PlacedClaims`]).
//! - **CoW destruction-safety** — every in-flight placed payload holds
//!   the assembly's shared handle, so the adopted cell is non-unique:
//!   in-place mutation (`make_mut`, `zero_complete`,
//!   `fill_complement_from`) copies away instead of touching severed
//!   bytes. A payload whose pointer proof fails at merge simply copies —
//!   its severed region is intact by construction.
//! - **Fallback-is-correctness** — every refusal (overlap, sealed, cap,
//!   shape) rides the pooled sever unchanged.
//!
//! Memory honesty: live assembly bytes are gauged
//! (`placed_assembly_bytes`) and R5-visible as the non-sheddable
//! `placed_assemblies` component (the `ipc_severed_buffers` pattern —
//! the budget must SEE the bytes; convergence is by adoption/drop, a
//! shed hook could not act on in-flight custody). Creation refuses past
//! the cap (`min(budget/8, 2 GiB)` — the session-shm cap shape).

use crate::cache::active_block::SharedBlock;
use crate::fuse_client::METRICS;
use crate::placed_core::{PlacedClaims, CLAIM_PAGE};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The assembly-bytes hard cap: the session-shm cap shape
/// (`min(budget/8, 2 GiB)`). Past it, placed severs refuse (pooled
/// fallback) — graceful, counted. Before the R5 authority arms (mount
/// worker not started — unit tests, early bring-up) the budget reads 0;
/// the cap then rests at the 2 GiB rail alone (assemblies stay
/// structurally bounded by in-flight ring ops — each holds ≤ one block
/// per live claim — and the gauge is registered the moment the host
/// arms).
fn assembly_cap_bytes() -> u64 {
    const RAIL: u64 = 2 * 1024 * 1024 * 1024;
    match crate::mem_budget::MEM_BUDGET.budget_bytes() {
        0 => RAIL,
        b => (b / 8).min(RAIL),
    }
}

/// One pre-adoption assembly: the shared backing + its claim state.
pub(crate) struct BlockAssembly {
    block: SharedBlock,
    claims: PlacedClaims,
    /// Live placed payload handles (claims not yet released) — the
    /// registry-reap gate.
    ///
    /// Ordering (PERF-21): the canonical `Arc`-class refcount discipline —
    /// the acquire increment is `Relaxed` (it happens under the registry's
    /// per-key entry guard, which already orders it against the reap that
    /// re-reads it), the release decrement is `Release`, and the
    /// last-out-the-door path pays one `Acquire` fence so the reaper
    /// observes every sibling payload's writes before it drops the
    /// backing. `SeqCst` was over-strong: nothing here needs a total
    /// order across unrelated locations (no Dekker pair — the
    /// sever-vs-adoption race is fenced inside
    /// [`crate::placed_core::PlacedClaims`], which keeps its documented
    /// `SeqCst` pair).
    outstanding: AtomicUsize,
}

impl Drop for BlockAssembly {
    fn drop(&mut self) {
        crate::gauge_core::sub_saturating(&METRICS.placed_assembly_bytes, self.block.len() as u64);
    }
}

/// The per-filesystem `(ino, block)` → assembly registry (latch-free).
pub(crate) struct PlacedSeverRegistry {
    map: scc::HashMap<(u64, u64), Arc<BlockAssembly>, ahash::RandomState>,
}

impl PlacedSeverRegistry {
    pub(crate) fn new() -> Self {
        Self {
            map: scc::HashMap::with_hasher(ahash::RandomState::new()),
        }
    }

    /// The placed sever: claim `[rel, rel + len)` of `(ino, block)`'s
    /// assembly (creating it when absent, under the cap), copy the arena
    /// window in (the ONE §5.5.2 arena read), and return the payload
    /// `Bytes` backed by the assembly region. `None` ⇒ the caller falls
    /// back to the pooled sever.
    ///
    /// # Safety
    ///
    /// * `src` must be valid for `len` byte reads for the duration of the
    ///   call (the arena window is alive across the synchronous dequeue —
    ///   torn content from racing client writes is the client's own
    ///   POSIX-legal race, exactly like the pooled sever).
    /// * `rel` and `len` must be [`CLAIM_PAGE`] multiples with
    ///   `rel + len <= block_size`. This is the only bound on the
    ///   `write_at` copy below (MEM-7a) — the claim bitmap indexes by page,
    ///   so a misaligned range grants a claim that does not cover what it
    ///   writes. The screen below now REFUSES such a range in every build
    ///   (it used to be a `debug_assert!` plus a screen inside the single
    ///   caller, i.e. nothing in a shipped binary).
    pub(crate) unsafe fn sever(
        self: &Arc<Self>,
        ino: u64,
        block: u64,
        rel: usize,
        len: usize,
        block_size: usize,
        src: *const u8,
    ) -> Option<bytes::Bytes> {
        // MEM-7a: enforced, not merely asserted — three integer tests on a
        // path that then copies up to a megabyte through a raw pointer.
        // Refusal is the pooled-sever fallback, exactly like a claim
        // overlap.
        if rel % CLAIM_PAGE != 0
            || len % CLAIM_PAGE != 0
            || len == 0
            || rel.saturating_add(len) > block_size
        {
            // Loud-never-fatal (the RES-22 law: a contract violation is a
            // counted tripwire, never a panic on a data path).
            log::error!(
                "placed sever refused: range must be CLAIM_PAGE-aligned and \
                 in-bounds (ino {ino} block {block} rel {rel} len {len} \
                 block_size {block_size}) — pooled sever fallback"
            );
            METRICS
                .ipc_placed_sever_fallbacks
                .fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let key = (ino, block);
        // Get-or-create + claim + outstanding++ under the entry guard
        // (serializes against the payload-drop reap of the same key).
        let assembly = {
            let entry = self.map.entry_sync(key);
            let assembly = match entry {
                scc::hash_map::Entry::Occupied(o) => Arc::clone(o.get()),
                scc::hash_map::Entry::Vacant(v) => {
                    // Cap check at creation only (an existing assembly is
                    // already-charged custody).
                    let charged = METRICS
                        .placed_assembly_bytes
                        .fetch_add(block_size as u64, Ordering::Relaxed)
                        + block_size as u64;
                    if charged > assembly_cap_bytes() {
                        crate::gauge_core::sub_saturating(
                            &METRICS.placed_assembly_bytes,
                            block_size as u64,
                        );
                        METRICS
                            .ipc_placed_sever_fallbacks
                            .fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                    let a = Arc::new(BlockAssembly {
                        block: SharedBlock::alloc(block_size),
                        claims: PlacedClaims::new(block_size),
                        outstanding: AtomicUsize::new(0),
                    });
                    v.insert_entry(Arc::clone(&a));
                    a
                }
            };
            if !assembly
                .claims
                .begin_claim(rel / CLAIM_PAGE, len / CLAIM_PAGE)
            {
                // Overlap with a live claim or a sealed (post-adoption)
                // assembly: pooled fallback (correctness owns ambiguity).
                METRICS
                    .ipc_placed_sever_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
                return None;
            }
            // Relaxed: published under the entry guard held here, which is
            // the same latch the reap's `remove_if_sync` takes (PERF-21).
            assembly.outstanding.fetch_add(1, Ordering::Relaxed);
            assembly
        };
        // The ONE arena read — outside the entry guard (a 1 MiB memcpy
        // must not hold the per-key latch), inside the claims writer
        // window (the adoption Dekker fences against it).
        // SAFETY: exclusive claim over [rel, rel+len) just granted; src
        // per the caller contract; bounds debug-asserted above and
        // enforced by the claim range.
        assembly.block.write_at(rel, src, len);
        assembly.claims.end_write();
        METRICS.ipc_placed_severs.fetch_add(1, Ordering::Relaxed);
        Some(bytes::Bytes::from_owner(PlacedSevered {
            registry: Arc::clone(self),
            key,
            assembly,
            rel,
            len,
        }))
    }

    /// The adoption probe (called by the write merge under the block's
    /// `BLOCK_FLUSH_LOCKS` guard, entry-absent branch): if `(ino, block)`
    /// has an assembly whose region at `rel` IS `payload_ptr` (pointer
    /// proof — only a placed sever can satisfy it), seal it against
    /// further claims and, when no sever is mid-copy (the Dekker), remove
    /// it from the registry and hand the backing over for
    /// [`crate::cache::active_block::ActiveBlockBuf::adopted`]. `None` ⇒
    /// the merge copies as before.
    pub(crate) fn take_for_adoption(
        &self,
        ino: u64,
        block: u64,
        payload_ptr: *const u8,
        rel: usize,
    ) -> Option<SharedBlock> {
        let key = (ino, block);
        let mut shared = None;
        self.map.remove_if_sync(&key, |a| {
            let matches = rel < a.block.len()
                // SAFETY: in-bounds arithmetic (rel < len); compared only.
                && std::ptr::eq(unsafe { a.block.as_ptr().add(rel) }, payload_ptr);
            if matches && a.claims.seal_for_adoption() {
                shared = Some(a.block.clone());
                true
            } else {
                // Pointer mismatch (foreign payload) or a sever mid-copy:
                // never adopt. A sealed non-adopted assembly drains via
                // payload drops; a mismatch keeps serving its own ops.
                if matches {
                    // The cohort-break gauge (Approach A): OUR assembly,
                    // but a writer window (bridge/sever mid-copy) blocked
                    // the seal — the whole cohort demotes to the copy
                    // path. Growth prices the quiescence window.
                    METRICS
                        .placed_adoption_refusals
                        .fetch_add(1, Ordering::Relaxed);
                }
                false
            }
        });
        shared
    }

    /// Payload-drop reap: release the claim and drop the registry entry
    /// once no placed payload references the (never-adopted) assembly.
    fn reap(&self, key: (u64, u64), assembly: &Arc<BlockAssembly>) {
        self.map.remove_if_sync(&key, |a| {
            // Acquire: pairs with every payload drop's `Release` decrement
            // (a racing successor claim re-published under this same entry
            // guard reads back nonzero and keeps the assembly alive).
            Arc::ptr_eq(a, assembly) && a.outstanding.load(Ordering::Acquire) == 0
        });
    }

    /// The FUSE placed-merge PLACEMENT (Approach A, write-bandwidth
    /// program 2026-08-09): claim `[rel, rel + len)` of `(ino, block)`'s
    /// **memfd-backed** assembly and open the writer window — the writer
    /// is the transport queue worker's `WRITE_FIXED(slot → assembly fd)`
    /// bridge, whose full-length CQE closes the window (the returned
    /// guard). Unlike [`Self::sever`] there is NO copy here: the claim's
    /// begin/end straddle an ASYNC kernel write, which is exactly what
    /// makes the §5.2 isolation law hold — [`Self::take_for_adoption`]'s
    /// `seal_for_adoption` refuses while any writer window is open, so
    /// **no kernel write can target the assembly after it becomes
    /// snapshot-visible** (pinned by
    /// `placement_writer_window_blocks_adoption` below).
    ///
    /// `None` ⇒ the caller falls back to the extraction vehicle:
    /// misaligned/out-of-range shape, an existing NON-memfd assembly
    /// (the IPC pooled kind — the bridge cannot target it), the
    /// assembly cap, a claim overlap, or a sealed (adopted) assembly.
    pub(crate) fn begin_placement(
        self: &Arc<Self>,
        ino: u64,
        block: u64,
        rel: usize,
        len: usize,
        block_size: usize,
    ) -> Option<FusePlacement> {
        if rel % CLAIM_PAGE != 0
            || len % CLAIM_PAGE != 0
            || len == 0
            || rel.saturating_add(len) > block_size
        {
            METRICS
                .ipc_placed_sever_fallbacks
                .fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let key = (ino, block);
        let assembly = {
            let entry = self.map.entry_sync(key);
            let assembly = match entry {
                scc::hash_map::Entry::Occupied(o) => Arc::clone(o.get()),
                scc::hash_map::Entry::Vacant(v) => {
                    let charged = METRICS
                        .placed_assembly_bytes
                        .fetch_add(block_size as u64, Ordering::Relaxed)
                        + block_size as u64;
                    if charged > assembly_cap_bytes() {
                        crate::gauge_core::sub_saturating(
                            &METRICS.placed_assembly_bytes,
                            block_size as u64,
                        );
                        METRICS
                            .ipc_placed_sever_fallbacks
                            .fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                    let Some((block_mem, _fd)) = SharedBlock::alloc_memfd(block_size) else {
                        crate::gauge_core::sub_saturating(
                            &METRICS.placed_assembly_bytes,
                            block_size as u64,
                        );
                        METRICS
                            .ipc_placed_sever_fallbacks
                            .fetch_add(1, Ordering::Relaxed);
                        return None;
                    };
                    let a = Arc::new(BlockAssembly {
                        block: block_mem,
                        claims: PlacedClaims::new(block_size),
                        outstanding: AtomicUsize::new(0),
                    });
                    v.insert_entry(Arc::clone(&a));
                    a
                }
            };
            // The bridge needs an fd: an existing POOLED assembly (the
            // IPC sever kind) cannot be targeted — fall back.
            if assembly.block.memfd_raw().is_none() {
                METRICS
                    .ipc_placed_sever_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
                return None;
            }
            if !assembly
                .claims
                .begin_claim(rel / CLAIM_PAGE, len / CLAIM_PAGE)
            {
                METRICS
                    .ipc_placed_sever_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
                return None;
            }
            // Relaxed: published under the entry guard (PERF-21).
            assembly.outstanding.fetch_add(1, Ordering::Relaxed);
            assembly
        };
        METRICS.placed_fuse_claims.fetch_add(1, Ordering::Relaxed);
        let fd = assembly
            .block
            .memfd_raw()
            .expect("memfd presence checked under the entry guard");
        let assembly_id = Arc::as_ptr(&assembly) as u64;
        // The writer-window closer, once-only (CQE success, CQE failure,
        // and every drop path all balance the SAME window).
        let end_assembly = Arc::clone(&assembly);
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let end_write: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if !ended.swap(true, Ordering::AcqRel) {
                end_assembly.claims.end_write();
            }
        });
        let payload = bytes::Bytes::from_owner(PlacedSevered {
            registry: Arc::clone(self),
            key,
            assembly,
            rel,
            len,
        });
        Some(FusePlacement {
            fd,
            file_off: rel as u64,
            payload,
            assembly_id,
            end_write,
        })
    }

    /// Test/teardown visibility: live assemblies.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

/// One granted FUSE placement (Approach A): the bridge target
/// (`fd`/`file_off`), the assembly-region payload the dispatch will
/// carry (claim released on drop), the cohort key, and the once-only
/// writer-window closer the CQE calls.
pub(crate) struct FusePlacement {
    pub(crate) fd: std::os::fd::RawFd,
    pub(crate) file_off: u64,
    pub(crate) payload: bytes::Bytes,
    pub(crate) assembly_id: u64,
    pub(crate) end_write: Arc<dyn Fn() + Send + Sync>,
}

/// The placed payload owner: `AsRef` = the assembly region; drop releases
/// the claim and reaps the registry entry at zero outstanding.
struct PlacedSevered {
    registry: Arc<PlacedSeverRegistry>,
    key: (u64, u64),
    assembly: Arc<BlockAssembly>,
    rel: usize,
    len: usize,
}

impl AsRef<[u8]> for PlacedSevered {
    fn as_ref(&self) -> &[u8] {
        self.assembly.block.region(self.rel, self.len)
    }
}

impl Drop for PlacedSevered {
    fn drop(&mut self) {
        self.assembly
            .claims
            .release(self.rel / CLAIM_PAGE, self.len / CLAIM_PAGE);
        if self.assembly.outstanding.fetch_sub(1, Ordering::Release) == 1 {
            // The last handle out: acquire everything the siblings released
            // before this thread decides the assembly is reapable.
            std::sync::atomic::fence(Ordering::Acquire);
            // Last payload out: reap the entry if it was never adopted
            // (adoption already removed it; ptr_eq keeps a racing
            // successor assembly safe).
            self.registry.reap(self.key, &self.assembly);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sever_ok(
        reg: &Arc<PlacedSeverRegistry>,
        ino: u64,
        block: u64,
        rel: usize,
        payload: &[u8],
        block_size: usize,
    ) -> Option<bytes::Bytes> {
        // SAFETY: payload is a live slice for the call.
        unsafe { reg.sever(ino, block, rel, payload.len(), block_size, payload.as_ptr()) }
    }

    /// THE §5.2 isolation-law pin (Approach A): a placement whose bridge
    /// write is still in flight (writer window OPEN — `end_write` not
    /// yet called) must BLOCK adoption, because adoption makes the
    /// assembly snapshot-visible and a late kernel write would mutate
    /// frozen bytes. After `end_write`, adoption proceeds; after
    /// adoption (sealed), no new placement may claim the assembly.
    #[test]
    fn placement_writer_window_blocks_adoption() {
        let reg = Arc::new(PlacedSeverRegistry::new());
        const BS: usize = 64 * 1024;
        let p = reg
            .begin_placement(9, 4, 0, 8192, BS)
            .expect("placement claim");
        // Bridge in flight: adoption MUST refuse (writer window open).
        assert!(
            reg.take_for_adoption(9, 4, p.payload.as_ptr(), 0).is_none(),
            "adoption while a kernel bridge write is in flight would let \
             the write land in snapshot-visible memory — the §5.2 law"
        );
        // CQE lands: the window closes (once-only — a second call is a
        // no-op, the guard/drop paths all balance the same window).
        (p.end_write)();
        (p.end_write)();
        let shared = reg
            .take_for_adoption(9, 4, p.payload.as_ptr(), 0)
            .expect("adoption after the writer window closed");
        assert!(
            shared.memfd_raw().is_some(),
            "the adopted backing is the memfd assembly"
        );
        // Post-adoption: the adopted assembly LEFT the registry (removal
        // IS the seal's registry face), so a later placement mints a
        // FRESH private assembly — the adopted, snapshot-visible backing
        // can never be a bridge target again (the §5.2 law's second
        // half; the MOUNT-level routing gate additionally refuses
        // placement while the adopted overlay entry lives — pinned by
        // `post_adoption_writes_never_target_the_assembly`).
        let p2 = reg
            .begin_placement(9, 4, 16384, 8192, BS)
            .expect("post-adoption placement mints a fresh assembly");
        assert!(
            !std::ptr::eq(p2.payload.as_ptr(), unsafe { shared.as_ptr().add(16384) }),
            "a post-adoption placement must never target the adopted \
             (snapshot-visible) backing"
        );
        drop(p2);
        drop(p);
    }

    /// Placement payload bytes ARE the assembly region (fd + VA views of
    /// one memory): bytes written through the memfd are the payload's.
    #[test]
    fn placement_payload_is_the_memfd_region() {
        let reg = Arc::new(PlacedSeverRegistry::new());
        const BS: usize = 64 * 1024;
        let p = reg
            .begin_placement(11, 2, 4096, 8192, BS)
            .expect("placement claim");
        let pattern = vec![0x7Eu8; 8192];
        // The kernel-bridge stand-in: pwrite through the fd at file_off.
        let n = unsafe {
            libc::pwrite(
                p.fd,
                pattern.as_ptr().cast(),
                8192,
                p.file_off as libc::off_t,
            )
        };
        assert_eq!(n, 8192);
        (p.end_write)();
        assert_eq!(
            &p.payload[..],
            &pattern[..],
            "fd writes are visible through the payload's mmap view"
        );
        // Pool-backed assembly (IPC sever) refuses placements: bridge
        // has no fd to target.
        let ipc = sever_ok(&reg, 12, 1, 0, &pattern, BS).expect("ipc sever");
        assert!(
            reg.begin_placement(12, 1, 16384, 8192, BS).is_none(),
            "a pooled (fd-less) assembly must refuse placements"
        );
        drop(ipc);
        drop(p);
    }

    #[test]
    fn placed_payload_reads_back_and_reaps_at_zero() {
        let reg = Arc::new(PlacedSeverRegistry::new());
        let a = vec![0xA1u8; 8192];
        let b = vec![0xB2u8; 4096];
        let pa = sever_ok(&reg, 7, 3, 0, &a, 64 * 1024).expect("first claim");
        let pb = sever_ok(&reg, 7, 3, 8192, &b, 64 * 1024).expect("disjoint claim");
        assert_eq!(&pa[..], &a[..], "payload view is the severed bytes");
        assert_eq!(&pb[..], &b[..]);
        assert_eq!(reg.len(), 1, "one shared assembly per (ino, block)");
        // Overlap refuses.
        assert!(
            sever_ok(&reg, 7, 3, 4096, &b, 64 * 1024).is_none(),
            "overlapping live claim must refuse"
        );
        drop(pa);
        assert_eq!(reg.len(), 1, "assembly lives while any payload does");
        drop(pb);
        assert_eq!(reg.len(), 0, "last payload drop reaps the assembly");
        // Region is claimable again through a FRESH assembly.
        let pc = sever_ok(&reg, 7, 3, 4096, &b, 64 * 1024).expect("fresh assembly");
        drop(pc);
    }

    /// MEM-7a: the page-alignment / in-bounds precondition is the ONLY
    /// release-build bound on `write_at`'s copy, and it lived in a
    /// `debug_assert!` plus a screen in the single caller — nothing at all
    /// in a shipped binary, and nothing for a second caller.
    #[test]
    fn a_misaligned_or_out_of_bounds_sever_is_refused_in_every_build() {
        let reg = Arc::new(PlacedSeverRegistry::new());
        let src = vec![0xEEu8; 8192];
        const BS: usize = 64 * 1024;
        // SAFETY (all four): `src` is a live slice for each call's duration.
        unsafe {
            assert!(
                reg.sever(7, 0, 1, 4096, BS, src.as_ptr()).is_none(),
                "a `rel` that is not CLAIM_PAGE-aligned must be refused — the \
                 claim bitmap indexes by page, so the copy is otherwise \
                 unbounded by anything"
            );
            assert!(
                reg.sever(7, 0, 0, 4095, BS, src.as_ptr()).is_none(),
                "a non-page-multiple `len` must be refused"
            );
            assert!(
                reg.sever(7, 0, BS - 4096, 8192, BS, src.as_ptr()).is_none(),
                "`rel + len > block_size` must be refused — the assembly is \
                 exactly one block"
            );
            // The screen is a BOUND, not a ban: the well-formed shape works.
            let ok = reg
                .sever(7, 0, 0, 8192, BS, src.as_ptr())
                .expect("an aligned, in-bounds sever still succeeds");
            assert_eq!(&ok[..], &src[..]);
        }
    }

    #[test]
    fn adoption_takes_the_backing_and_seals_new_claims_out() {
        let reg = Arc::new(PlacedSeverRegistry::new());
        let a = vec![0x5Au8; 4096];
        let pa = sever_ok(&reg, 9, 1, 4096, &a, 64 * 1024).expect("claim");
        let shared = reg
            .take_for_adoption(9, 1, pa.as_ptr(), 4096)
            .expect("pointer proof adopts");
        assert_eq!(shared.region(4096, 4096), &a[..]);
        assert_eq!(reg.len(), 0, "adoption removes the registry entry");
        // The payload stays readable after adoption (shared handle).
        assert_eq!(&pa[..], &a[..]);
        // A foreign pointer never adopts.
        assert!(
            reg.take_for_adoption(9, 1, a.as_ptr(), 4096).is_none(),
            "no entry / foreign pointer must not adopt"
        );
        drop(pa);
    }

    #[test]
    fn foreign_pointer_never_adopts_a_live_assembly() {
        let reg = Arc::new(PlacedSeverRegistry::new());
        let a = vec![0x11u8; 4096];
        let pa = sever_ok(&reg, 4, 2, 0, &a, 64 * 1024).expect("claim");
        let foreign = vec![0x22u8; 4096];
        assert!(
            reg.take_for_adoption(4, 2, foreign.as_ptr(), 0).is_none(),
            "a pooled payload's pointer must fail the proof"
        );
        assert_eq!(reg.len(), 1, "mismatch must not remove the assembly");
        // The real payload still adopts.
        assert!(reg.take_for_adoption(4, 2, pa.as_ptr(), 0).is_some());
        drop(pa);
    }
}
