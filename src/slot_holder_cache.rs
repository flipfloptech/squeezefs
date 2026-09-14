//! **`SlotHolderCache`** — slot → holder resolution as a control-plane
//! projection (docs/design-symmetric-metadata.md §5.1.6, KD-SYM-17; PR 4).
//!
//! Tree 0 records the LESSEE of every leased slot (`Leased { appender_id,
//! g, … }`), so resolving a slot to its token server / ship target needs
//! no manager RPC: the cache is fed from tree 0 (at the arm, at every
//! grant and release the manager writes, at a reader's epoch poll) and
//! read latch-free — an `ArcSwap`'d map, the `PlacementTable` pattern.
//! `ResolveSlot` to the volume's manager is the STALE-VIEW fallback,
//! counted `slot_resolve_rpcs` (NOT `dlm_rpcs`, which keeps its S4
//! meaning — lock round trips); a holder answering `NotHolder { current }`
//! redirects (`slot_resolve_redirects`) and the entry is replaced.
//! Invalidation: an observed `g` change replaces the entry; a dead member
//! (the grant's `dead_members`, PR 8/10) drops every entry it held.
//!
//! The holder's ENDPOINT is the membership census's — the appender id →
//! member identity binding is PR 12's join ladder; until then the cache
//! answers the appender id and `g`, which is what the in-process manager
//! and the contracts consume.

use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// One resolved slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotHolder {
    pub appender_id: u32,
    /// The lease generation the entry was learnt at — a newer observed
    /// `g` replaces the entry.
    pub g: u32,
}

/// The per-volume cache.
#[derive(Debug, Default)]
pub struct SlotHolderCache {
    map: ArcSwap<HashMap<u32, SlotHolder>>,
    resolve_rpcs: AtomicU64,
    redirects: AtomicU64,
}

impl SlotHolderCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole view (tree 0's lessee population at a poll /
    /// the arm): `entries` = `(forest slot, appender_id, g)`.
    pub fn refresh(&self, entries: impl IntoIterator<Item = (u32, u32, u32)>) {
        let map: HashMap<u32, SlotHolder> = entries
            .into_iter()
            .map(|(slot, appender_id, g)| (slot, SlotHolder { appender_id, g }))
            .collect();
        self.map.store(Arc::new(map));
    }

    /// One slot learnt (a grant this manager wrote; a redirect's
    /// `current`; a resolve's answer). A lower `g` than the cached one is
    /// a stale view and is ignored.
    pub fn learn(&self, slot: u32, holder: SlotHolder) {
        let cur = self.map.load();
        if cur.get(&slot).is_some_and(|h| h.g > holder.g) {
            return;
        }
        let mut next = (**cur).clone();
        next.insert(slot, holder);
        self.map.store(Arc::new(next));
    }

    /// The slot is unleased (a release observed).
    pub fn forget(&self, slot: u32) {
        let cur = self.map.load();
        if !cur.contains_key(&slot) {
            return;
        }
        let mut next = (**cur).clone();
        next.remove(&slot);
        self.map.store(Arc::new(next));
    }

    /// Every entry `appender_id` held (a dead member's).
    pub fn invalidate_holder(&self, appender_id: u32) {
        let cur = self.map.load();
        if !cur.values().any(|h| h.appender_id == appender_id) {
            return;
        }
        let mut next = (**cur).clone();
        next.retain(|_, h| h.appender_id != appender_id);
        self.map.store(Arc::new(next));
    }

    /// The cached holder — one lock-free load (`None` = unleased as far
    /// as this view knows).
    pub fn holder(&self, slot: u32) -> Option<SlotHolder> {
        self.map.load().get(&slot).copied()
    }

    /// Resolve `slot`: the cached view first; a miss runs `fallback` (the
    /// `ResolveSlot` verb to the manager — counted `slot_resolve_rpcs`)
    /// and learns its answer.
    pub fn resolve(
        &self,
        slot: u32,
        fallback: impl FnOnce() -> Option<SlotHolder>,
    ) -> Option<SlotHolder> {
        if let Some(h) = self.holder(slot) {
            return Some(h);
        }
        self.resolve_rpcs.fetch_add(1, Ordering::Relaxed);
        let h = fallback()?;
        self.learn(slot, h);
        Some(h)
    }

    /// A holder answered `NotHolder { current }` (§5.1.6): replace the
    /// stale entry (`slot_resolve_redirects`).
    pub fn redirect(&self, slot: u32, current: Option<SlotHolder>) {
        self.redirects.fetch_add(1, Ordering::Relaxed);
        match current {
            Some(h) => {
                let mut next = (**self.map.load()).clone();
                next.insert(slot, h);
                self.map.store(Arc::new(next));
            }
            None => self.forget(slot),
        }
    }

    /// `slot_resolve_rpcs`.
    pub fn resolve_rpcs(&self) -> u64 {
        self.resolve_rpcs.load(Ordering::Relaxed)
    }

    /// `slot_resolve_redirects`.
    pub fn redirects(&self) -> u64 {
        self.redirects.load(Ordering::Relaxed)
    }

    /// Entries cached.
    pub fn len(&self) -> usize {
        self.map.load().len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.map.load().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_path_never_falls_back_and_redirects_replace() {
        let c = SlotHolderCache::new();
        c.refresh([(1, 7, 3), (2, 8, 1)]);
        assert_eq!(
            c.resolve(1, || panic!("warm")),
            Some(SlotHolder {
                appender_id: 7,
                g: 3
            })
        );
        assert_eq!(c.resolve_rpcs(), 0);
        assert_eq!(
            c.resolve(9, || Some(SlotHolder {
                appender_id: 1,
                g: 1
            })),
            Some(SlotHolder {
                appender_id: 1,
                g: 1
            })
        );
        assert_eq!(c.resolve_rpcs(), 1);
        // A stale learn (lower g) is ignored; a redirect replaces.
        c.learn(
            1,
            SlotHolder {
                appender_id: 5,
                g: 2,
            },
        );
        assert_eq!(c.holder(1).map(|h| h.appender_id), Some(7));
        c.redirect(
            1,
            Some(SlotHolder {
                appender_id: 5,
                g: 4,
            }),
        );
        assert_eq!(c.holder(1).map(|h| h.appender_id), Some(5));
        assert_eq!(c.redirects(), 1);
        c.invalidate_holder(5);
        assert_eq!(c.holder(1), None);
        c.forget(2);
        assert_eq!(c.len(), 1);
    }
}
