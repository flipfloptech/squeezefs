//! **`SlotHolderCache`** — slot → holder resolution as a control-plane
//! projection (docs/design-symmetric-metadata.md §5.1.6, KD-SYM-17; PR 4).
//!
//! Tree 0 records the LESSEE of every leased slot (`Leased { appender_id,
//! g, … }`), so resolving a slot to its token server / ship target needs
//! no manager RPC: the cache is fed from tree 0 (rebuilt at the arm and at
//! a reader's epoch poll — [`SlotHolderCache::refresh`]; one entry learnt
//! at every grant and forgotten at every release the manager writes —
//! [`SlotHolderCache::learn`] / [`SlotHolderCache::forget`]) and read
//! latch-free — an `ArcSwap`'d map, the `PlacementTable` pattern. The
//! stale-view fallback (`ResolveSlot` to the volume's manager, counted
//! `slot_resolve_rpcs` on the SERVER — NOT `dlm_rpcs`, which keeps its S4
//! meaning) and the `NotHolder { current }` redirect are the ship path's
//! (PR 6/12), which is where they land with their first caller; a dead
//! member's invalidation is PR 10's. Nothing of them is built ahead of
//! that caller (the no-dead-code law).
//!
//! The holder's ENDPOINT is the membership census's — the appender id →
//! member identity binding is the join ladder's rung 7 (PR 12:
//! `sym_join::bind_live_appender_endpoints`, every Live appender's published
//! listener; a later joiner's arrives with PR 12b's wire `JoinAppender`). PR 6 gave the cache the
//! endpoint TABLE that binding fills ([`SlotHolderCache::set_endpoint`] /
//! [`SlotHolderCache::endpoint`]): the cross-owner step shipper resolves a
//! foreign slot's holder to its appender id here and to a wire endpoint
//! there; a holder with no endpoint yet is the un-shippable class the
//! roll-forward cadence retries (design §5.6, `xv_cross_owner_intents_
//! stuck`). The contracts fill the table directly; the join ladder is
//! its product writer.

use arc_swap::ArcSwap;
use std::collections::HashMap;
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
    /// Appender id → the wire endpoint its owner service listens on.
    endpoints: ArcSwap<HashMap<u32, Arc<str>>>,
    /// Appender id → its MEMBER id (`cowriter::node_member_id_of` over its
    /// page identity) — the liveness key the S6 owner answers for (PR 12b
    /// round 3, F2: a redirect to a holder the owner no longer lists live
    /// is never handed out).
    members: ArcSwap<HashMap<u32, Arc<str>>>,
}

impl SlotHolderCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `appender_id` to the endpoint its owner service listens on
    /// (the join ladder's census binding; the contracts' seam).
    pub fn set_endpoint(&self, appender_id: u32, endpoint: &str) {
        let cur = self.endpoints.load();
        let mut next = (**cur).clone();
        next.insert(appender_id, Arc::from(endpoint));
        self.endpoints.store(Arc::new(next));
    }

    /// The endpoint bound to `appender_id` — one lock-free load.
    pub fn endpoint(&self, appender_id: u32) -> Option<Arc<str>> {
        self.endpoints.load().get(&appender_id).cloned()
    }

    /// The appender bound to `endpoint` (the custody renewal loop's
    /// re-resolve key — PR 12b round 4, Issue 22); the lowest id when one
    /// listener serves several appenders of a daemon (one page identity,
    /// one claim-set entry — every id at the address resolves alike).
    pub fn appender_at_endpoint(&self, endpoint: &str) -> Option<u32> {
        self.endpoints
            .load()
            .iter()
            .filter(|(_, bound)| bound.as_ref() == endpoint)
            .map(|(id, _)| *id)
            .min()
    }

    /// Remember `appender_id`'s member id (learnt wherever its page
    /// identity is in hand: the ladder's census binding, a served join or
    /// publish).
    pub fn set_member_id(&self, appender_id: u32, member: &str) {
        if self
            .members
            .load()
            .get(&appender_id)
            .is_some_and(|m| **m == *member)
        {
            return;
        }
        let cur = self.members.load();
        let mut next = (**cur).clone();
        next.insert(appender_id, Arc::from(member));
        self.members.store(Arc::new(next));
    }

    /// The member id remembered for `appender_id` — one lock-free load.
    pub fn member_id(&self, appender_id: u32) -> Option<Arc<str>> {
        self.members.load().get(&appender_id).cloned()
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

    /// One slot learnt (a grant this manager wrote). A lower `g` than
    /// the cached one is a stale view and is ignored.
    pub fn learn(&self, slot: u32, holder: SlotHolder) {
        let cur = self.map.load();
        if cur.get(&slot).is_some_and(|h| h.g > holder.g) {
            return;
        }
        let mut next = (**cur).clone();
        next.insert(slot, holder);
        self.map.store(Arc::new(next));
    }

    /// The slot is unleased (a release this manager wrote).
    pub fn forget(&self, slot: u32) {
        let cur = self.map.load();
        if !cur.contains_key(&slot) {
            return;
        }
        let mut next = (**cur).clone();
        next.remove(&slot);
        self.map.store(Arc::new(next));
    }

    /// The cached holder — one lock-free load (`None` = unleased as far
    /// as this view knows).
    pub fn holder(&self, slot: u32) -> Option<SlotHolder> {
        self.map.load().get(&slot).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_learn_and_forget_keep_the_newest_view() {
        let c = SlotHolderCache::new();
        c.refresh([(1, 7, 3), (2, 8, 1)]);
        assert_eq!(
            c.holder(1),
            Some(SlotHolder {
                appender_id: 7,
                g: 3
            })
        );
        assert_eq!(c.holder(9), None);
        // A stale learn (lower g) is ignored; a newer one replaces.
        c.learn(
            1,
            SlotHolder {
                appender_id: 5,
                g: 2,
            },
        );
        assert_eq!(c.holder(1).map(|h| h.appender_id), Some(7));
        c.learn(
            1,
            SlotHolder {
                appender_id: 5,
                g: 4,
            },
        );
        assert_eq!(c.holder(1).map(|h| h.appender_id), Some(5));
        c.forget(2);
        assert_eq!(c.holder(2), None);
        c.forget(2);
        assert_eq!(c.holder(1).map(|h| h.g), Some(4));
    }
}
