//! **The shipped-free wire** — the displaced half of a rewrite on a mount
//! that is not the data volume's allocation-lease HOLDER (design-symmetric-
//! metadata §5.5, PR 8/12b; wire `crate::meta_ship::publish`,
//! `PublishCall::FreeBlocks`).
//!
//! The split, in one paragraph: a free's DURABLE effect (the
//! `TREE_BLOCK_REFS` delete) already rides the layout publish that
//! displaced the block — whole-tx, the ordering point. What is left is the
//! ACCOUNTING ladder (`begin_free` → tier purge → reclaim enqueue →
//! `finish_free`, with §6.8 item 3's grace ring and S7's quarantine
//! composing inside `finish_free`, ending in the allocation bitmap's CLEAR
//! on a grant-armed allocator), whose durable home is the holder's ledger
//! and whose device reclaimer is live only there. So a joined writer's
//! router-level terminal free SHIPS as a verb and the HOLDER executes the
//! whole ladder exactly as if it had freed locally.
//!
//! Two halves, both here: [`ship_displaced_frees`] (the shipper, presenting
//! PR 9's per-holder custody lease) and [`execute_shipped_frees`] (the
//! holder's executor, installed as the publish plane's `FreeExecutor`).
//! The recompute arm's LOCAL hygiene at the shipper — a publish the holder
//! recomputed frees the displaced blocks post-commit, so the shipper only
//! purges and retires ([`retire_recomputed_parked_key`],
//! [`retire_displaced_locally`]) — lives here too.

use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use crate::meta_backend::RoutedMetaBackend;
use std::sync::atomic::Ordering;
use std::sync::Arc;

squeezefs_ipc::sqz_task_local! {
    /// The **authority-accounting venue marker**: set for exactly one task
    /// tree — the shipped-free executor's ([`execute_shipped_frees`]) —
    /// and read only on the never-taken shipping branch of the
    /// ownership-accounting gates (`BlockAllocator::plane_gate`, the
    /// reclaim enqueue, the router's free seam).
    ///
    /// Why it exists: the holder's executor acts with the holder's
    /// accounting authority on behalf of a validated peer request. In
    /// production the two coincide; in any venue where one process plays
    /// both nodes, this marker is what keeps the executor's ladder from
    /// being mistaken for the shipper's own — and it is NEVER ambiently
    /// active.
    static AUTHORITY_FREE_SCOPE: ();
}

/// Run `fut` under the authority-accounting scope. Its callers are the
/// holder's own accounting acts performed while SERVING a peer — the
/// shipped-free executor and the served compose's displaced-blob free
/// (`multi_writer::indirect_map_io_for`); nothing else may enter it.
pub async fn with_authority_accounting<F>(fut: F) -> F::Output
where
    F: std::future::Future,
{
    AUTHORITY_FREE_SCOPE.scope((), fut).await
}

/// `true` ⇔ the current task runs under [`with_authority_accounting`].
/// Consulted only AFTER a gate already decided the free ships, so a
/// holder's own free never pays the task-local probe.
pub fn authority_accounting_scope_active() -> bool {
    AUTHORITY_FREE_SCOPE.try_with(|_| ()).is_ok()
}

/// The ship verbs' client-side request ids: monotone per process. The
/// witness is `(lease_epoch, request_id)` — the epoch scopes the id, so a
/// re-joined writer (a new join = a new epoch) can never alias a prior
/// incarnation's ids.
static FREE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Mint one client-side ship request id (one monotone sequence keeps ids
/// unique per process regardless of verb).
pub(crate) fn next_ship_request_id() -> u64 {
    FREE_SEQ.fetch_add(1, Ordering::AcqRel) + 1
}

/// Bounded resend budget for one free verb. A protocol constant, not a
/// resource cap: each resend is absorbed exactly-once by the holder's dedup
/// window, and past the budget the abandon is the LEAK-SAFE direction
/// (durably free, returned by the holder's next derivation) — counted loud
/// on `free_ship_failures`.
const FREE_SHIP_ATTEMPTS: u32 = 3;

/// One parked key's verdict under `DataRouter::retire_recomputed_parked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecomputedRetire {
    /// The holder did not free this key's block (or the key is not this
    /// router's) — it stays parked for the epoch close's arm.
    Kept,
    /// The key's lifetime is the offset's LIVE one and the holder freed the
    /// block: local tracking retired (`shipped_free.recomputed_retires`).
    Retired,
    /// The holder freed the block, but this mount has since RE-MINTED the
    /// offset (a grant beat the reply): the key names a dead lifetime — the
    /// live owner's entry and word are untouched, the key is dropped.
    DeadLifetime,
}

/// The per-key act of `DataRouter::retire_recomputed_parked` (the
/// recompute arm's local hygiene at the covering publish): resolve the
/// parked key against this router's allocator, match its `(vol_tag,
/// block_idx)` against the holder's `freed` set, guard the LIFETIME (a key
/// whose stamp is not the offset's live incarnation names a lifetime this
/// mount already re-minted — touched by nobody), and retire under the
/// `Freed`-verdict discipline of [`ship_displaced_frees`]: read tiers
/// purged, [`crate::block_allocator::BlockAllocator::
/// retire_shipped_free_tracking`] (entry gone, word retired and republished
/// under a new generation). Unstamped keys (`INCARNATION_NONE`) retire on
/// the match alone, exactly as [`retire_displaced_locally`] does.
pub fn retire_recomputed_parked_key(
    router: &crate::routing::BackendRouter,
    key: &str,
    freed: &[crate::meta_ship::publish::WireFreedBlock],
) -> RecomputedRetire {
    let cleaned = crate::routing::clean_block_key(key);
    let Ok(parts) = router.parse_block_key_parts(&cleaned) else {
        return RecomputedRetire::Kept;
    };
    let Some(alloc) = router.allocator_for_be_id(&parts.be_id) else {
        return RecomputedRetire::Kept;
    };
    let vol_tag = crate::meta_backend::kv::block_refs::volume_tag(alloc.volume_id());
    let block_idx = parts.offset / alloc.chunk_size().max(1);
    if !freed
        .iter()
        .any(|f| f.vol_tag == vol_tag && f.block_idx == block_idx)
    {
        return RecomputedRetire::Kept;
    }
    if parts.incarnation != crate::routing::INCARNATION_NONE {
        let live = alloc.live_incarnation(parts.offset);
        if live != crate::routing::INCARNATION_NONE && live != parts.incarnation {
            log::debug!(
                "recompute retire: parked key '{cleaned}' names a dead lifetime of offset {} \
                 (live {live:#x}) — the offset was re-minted here before the holder's reply; \
                 the live lifetime is untouched",
                parts.offset
            );
            return RecomputedRetire::DeadLifetime;
        }
    }
    // A peer's mint this rewrite displaced is untracked here by
    // construction: its word still retires, but only an entry that existed
    // is COUNTED.
    let tracked = alloc.refcount(parts.offset).is_some();
    router.purge_read_tiers(&cleaned);
    alloc.retire_shipped_free_tracking(parts.offset);
    if tracked {
        METRICS.recomputed_retires.fetch_add(1, Ordering::Relaxed);
    }
    RecomputedRetire::Retired
}

/// The LOCAL hygiene half of a displaced free whose DEVICE half the holder
/// already owns (a publish the holder RECOMPUTED ran the committed
/// transition's displaced blocks through its own ladder post-commit): the
/// read-tier purge and the local tracking release — no wire, no device
/// commands, no accounting. A frame key whose lifetime stamp is DEAD names
/// a prior lifetime of the offset this mount has since re-minted; the live
/// lifetime's tracking is not this key's to release.
pub fn retire_displaced_locally(router: &crate::routing::BackendRouter, block_keys: &[String]) {
    for key in block_keys {
        let cleaned = crate::routing::clean_block_key(key);
        router.purge_read_tiers(&cleaned);
        let Ok(parts) = router.parse_block_key_parts(&cleaned) else {
            continue; // the local ladder's silent skip (unparsable key)
        };
        if let Some(alloc) = router.allocator_for_be_id(&parts.be_id) {
            if parts.incarnation != crate::routing::INCARNATION_NONE {
                let live = alloc.live_incarnation(parts.offset);
                if live != crate::routing::INCARNATION_NONE && live != parts.incarnation {
                    continue;
                }
            }
            alloc.release_shipped_free_tracking(parts.offset);
        }
    }
}

/// The custody client at data volume `vol_tag`'s free target
/// (`data_grant::slot_holder_client`), re-resolving the target off durable
/// state ONCE when the dial fails (PR 12b round 3, F5): a failover the
/// writer's grant window covered moved the holder's listener with no grant
/// ask to notice it, so the first free after it met a dead address here.
fn holder_client_following(
    vol_tag: u64,
    target: String,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Arc<crate::data_grant::WriteCustodyClient>>> + Send,
    >,
> {
    Box::pin(async move {
        match crate::data_grant::slot_holder_client(&target).await {
            Ok(c) => Ok(c),
            Err(e) => match follow_moved_holder(vol_tag).await? {
                Some((_, c)) => Ok(c),
                None => Err(e),
            },
        }
    })
}

/// Re-resolve data volume `vol_tag`'s allocation holder off durable state;
/// a MOVED venue answers the new target and a custody client JOINed there.
/// Boxed: the free path's future is already deep inside the write
/// pipeline, and the auto-trait proof overflowed with this arm inlined.
fn follow_moved_holder(
    vol_tag: u64,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<Option<(String, Arc<crate::data_grant::WriteCustodyClient>)>>,
            > + Send,
    >,
> {
    Box::pin(async move {
        match crate::meta_backend::kv::alloc_lease::refresh_free_target(vol_tag).await {
            Some((moved_to, true)) => {
                let c = crate::data_grant::slot_holder_client(&moved_to).await?;
                Ok(Some((moved_to, c)))
            }
            _ => Ok(None),
        }
    })
}

/// **Ship a batch of displaced-block terminal frees to their data volume's
/// allocation-lease holder** — the shipping arm of
/// [`crate::routing::BackendRouter::free_block`] / `free_blocks` (the
/// write path's displacement calls, truncate's and unlink's).
///
/// Per key, in order:
/// 1. the same resolution the local ladder runs (decoration strip, parse,
///    stale-incarnation refusal, unknown-backend skip);
/// 2. the verb ships — `(vol_tag, block_idx)`, the durable identity, never
///    a path or a key string — under the CURRENT lease epoch at the holder
///    and one fresh request id, with bounded epoch-stable retries;
/// 3. only after the holder's acknowledgement: the LOCAL non-accounting
///    hygiene — the read-tier purge and the local tracking retire — so a
///    free that never shipped leaves this mount's state untouched
///    (leak-safe, re-derivable).
///
/// **A retry never re-keys.** It carries the SAME `(epoch, id)`; if the
/// lease epoch moves under it (revocation → re-join) the free is ABANDONED,
/// because a resend under the new epoch would be a new act the window
/// cannot correlate — the freed-then-reallocated ABA window. The abandoned
/// offset is durably unreferenced and recovery owns it.
pub async fn ship_displaced_frees(
    router: &crate::routing::BackendRouter,
    block_keys: &[&str],
) -> Result<()> {
    struct FreeGroup {
        alloc: Arc<crate::block_allocator::BlockAllocator>,
        vol_tag: u64,
        /// `(cleaned key, offset, block_idx)` per displaced block.
        entries: Vec<(String, u64, u64)>,
    }
    let mut groups: Vec<FreeGroup> = Vec::new();
    let mut first_err: Option<SqueezefsError> = None;
    for &key in block_keys {
        let cleaned = crate::routing::clean_block_key(key);
        let parts = match router.parse_block_key_parts(&cleaned) {
            Ok(p) => p,
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
                continue;
            }
        };
        // Spec §6.2 item 6: the same stale-lifetime refusal the local free
        // runs — a free under a dead incarnation is the §6.3 hazard's
        // destructive face wherever it executes.
        if !router.block_key_incarnation_ok(&cleaned) {
            if first_err.is_none() {
                first_err = Some(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "block key '{key}' names a dead incarnation of its device offset (spec \
                         §6.2 item 6) — refusing to ship its free"
                    ),
                )));
            }
            continue;
        }
        let Some(alloc) = router.allocator_for_be_id(&parts.be_id) else {
            // Unknown/offline backend: the local ladder's silent skip.
            continue;
        };
        let vol_tag = crate::meta_backend::kv::block_refs::volume_tag(alloc.volume_id());
        let idx = parts.offset / alloc.chunk_size().max(1);
        match groups
            .iter_mut()
            .find(|g| g.vol_tag == vol_tag && Arc::ptr_eq(&g.alloc, &alloc))
        {
            Some(g) => g.entries.push((cleaned, parts.offset, idx)),
            None => groups.push(FreeGroup {
                alloc,
                vol_tag,
                entries: vec![(cleaned, parts.offset, idx)],
            }),
        }
    }
    if groups.is_empty() {
        return match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        };
    }

    for group in groups {
        // PR 8 (design-symmetric-metadata §5.5): a data volume's terminal
        // frees ship to THAT volume's allocation-lease holder; the lease the
        // free presents is PR 9's per-holder custody client there, dialed
        // on demand for the target the grant arm named.
        let Some(mut target) = crate::block_grant::free_target_for(group.vol_tag) else {
            crate::meta_ship::publish::note_free_ship_failure(group.entries.len() as u64);
            let msg = format!(
                "{} displaced-block free(s) on vol_tag {:#016x} cannot ship — this mount holds \
                 no allocation lease on the volume and names no holder to execute the ladder. \
                 Nothing moved locally (leak-safe: the offsets are durably unreferenced and the \
                 holder's next derivation returns them)",
                group.entries.len(),
                group.vol_tag
            );
            log::error!("{msg}");
            if first_err.is_none() {
                first_err = Some(SqueezefsError::InvalidOperation(msg));
            }
            continue;
        };
        let mut client = match holder_client_following(group.vol_tag, target.clone()).await {
            Ok(c) => c,
            Err(e) => {
                crate::meta_ship::publish::note_free_ship_failure(group.entries.len() as u64);
                if first_err.is_none() {
                    first_err = Some(e);
                }
                continue;
            }
        };
        let mut epoch = client.lease_epoch();
        let mut request_id = next_ship_request_id();
        let idxs: Vec<u64> = group.entries.iter().map(|(_, _, idx)| *idx).collect();
        let mut attempt = 0u32;
        // The holder MOVED under us once already (a re-resolve at a
        // transport failure below): the ship is re-keyed ONCE — a fresh
        // client (its lease at the successor), a fresh request id — the
        // dead holder executed nothing under the old id.
        let mut followed = false;
        let shipped = loop {
            match crate::meta_ship::publish::ship_free_blocks(
                &target,
                group.vol_tag,
                idxs.clone(),
                epoch,
                request_id,
            )
            .await
            {
                Ok(verdicts) => break Ok(verdicts),
                Err(e) => {
                    attempt += 1;
                    // The holder's venue re-resolved off durable state at a
                    // TRANSPORT failure (PR 12b round 3, F5): a moved venue
                    // re-homes the free target and this ship follows it once
                    // with a client JOINed there — the old id never reached
                    // anyone, so the re-key correlates with nothing.
                    if !followed && crate::cluster_wire::is_transport_failure(&e) {
                        followed = true;
                        match follow_moved_holder(group.vol_tag).await {
                            Ok(Some((moved_to, c))) => {
                                client = c;
                                epoch = client.lease_epoch();
                                request_id = next_ship_request_id();
                                target = moved_to;
                                continue;
                            }
                            Ok(None) => {}
                            Err(je) => break Err(je),
                        }
                    }
                    // A retry NEVER re-keys: if the lease epoch moved (a
                    // revocation → re-join happened under us), abandon —
                    // the window cannot correlate a new epoch's resend with
                    // the old one's possible execution, and the ABA-safe
                    // direction is the leak-safe one.
                    if client.lease_epoch() != epoch || attempt >= FREE_SHIP_ATTEMPTS {
                        break Err(e);
                    }
                    squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        };
        match shipped {
            Ok(verdicts) => {
                // The holder owns the accounting now; retire this mount's
                // local, non-accounting view PER VERDICT (finding 19/20 —
                // the verdict-blind retire moved a LIVE owner's incarnation
                // word whenever a `Refused` double-release entry rode a
                // served batch):
                //
                // * `Freed` — the offset is dead: purge the read tiers (a
                //   reused offset must never tier-hit the dead
                //   incarnation's bytes) and retire refcount + word;
                // * `NonTerminal` — a sibling reference keeps the block
                //   alive: release THIS reference only, never the word; the
                //   tier purge stays (this ino's binding is gone — absence
                //   costs a refetch, never a wrong serve);
                // * `Refused` — the double-release lineage: the holder
                //   refused the act, and the offset's local state belongs to
                //   its CURRENT owner (possibly this mount's own re-mint).
                //   Touch NOTHING.
                for ((key, offset, _idx), verdict) in group.entries.iter().zip(verdicts.iter()) {
                    match verdict {
                        crate::meta_ship::publish::FreeVerdict::Freed => {
                            router.purge_read_tiers(key);
                            group.alloc.retire_shipped_free_tracking(*offset);
                        }
                        crate::meta_ship::publish::FreeVerdict::NonTerminal => {
                            router.purge_read_tiers(key);
                            group.alloc.release_shipped_free_tracking(*offset);
                        }
                        crate::meta_ship::publish::FreeVerdict::Refused => {}
                    }
                }
            }
            Err(e) => {
                crate::meta_ship::publish::note_free_ship_failure(group.entries.len() as u64);
                log::error!(
                    "ABANDONING {} displaced-block free(s) on vol_tag {:#016x} after {attempt} \
                     attempt(s) ({e}). The blocks are durably unreferenced (the publish landed) \
                     and stay out of every free list until the holder's next derivation — the \
                     leak-safe direction (free_ship_failures)",
                    group.entries.len(),
                    group.vol_tag,
                );
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// **The holder half: execute a peer's shipped frees** against this
/// holder's data plane — the [`crate::meta_ship::publish::FreeExecutor`]
/// body. Runs under [`with_authority_accounting`], because this ladder IS
/// the holder's own accounting act.
///
/// Per block, the verdict derivation — RAM first, the durable ledger for
/// what RAM never tracked:
///
/// * **RAM-tracked, durably justified** (`population ≥ refcount` —
///   finding 23's SHIELD): the shipper names a DEAD lifetime of a
///   freed-and-reallocated offset (a legitimate free's release rode its
///   publish, so population runs below refcount) — `Refused`, counted on
///   `block_live_free_refusals`, the live successor untouched;
/// * **RAM-tracked** (this mount minted or recovered it): the standard
///   [`crate::routing::BackendRouter::free_block`] ladder runs and the
///   refcount decides terminal vs not — byte-identical to a local free;
/// * **untracked, ledger population > 0**: `NonTerminal` — the reference
///   release already happened durably on the publish, and there is no RAM
///   state to move;
/// * **untracked, population 0, not already free/graced/quarantined/
///   mid-reclaim**: seed ONE reference ([`crate::block_allocator::
///   BlockAllocator::seed_shipped_free_reference`] — deliberately NOT
///   `recover_block`, whose gap-filling arm would declare a live peer's
///   unpublished tail free) and run the ladder — `Freed`;
/// * **anything else**: the double-release lineage — routed through the
///   ladder UNSEEDED so the existing untracked-free tripwire counts it,
///   and answered `Refused`.
pub async fn execute_shipped_frees(
    backend: &Arc<crate::routing::BackendRouter>,
    meta: &Arc<RoutedMetaBackend>,
    vol_tag: u64,
    blocks: &[u64],
) -> Result<Vec<crate::meta_ship::publish::FreeVerdict>> {
    use crate::meta_ship::publish::FreeVerdict;
    let Some((be_id, alloc)) = backend.allocator_for_volume_tag(vol_tag) else {
        return Err(SqueezefsError::InvalidOperation(format!(
            "a shipped free names data volume tag {vol_tag:#016x}, which this holder routes no \
             allocator for — refusing rather than freeing on a guessed volume"
        )));
    };
    let backend = Arc::clone(backend);
    let meta = Arc::clone(meta);
    let blocks = blocks.to_vec();
    with_authority_accounting(async move {
        let chunk = alloc.chunk_size();
        let mut verdicts = Vec::with_capacity(blocks.len());
        // ONE ledger read for the whole batch — the untracked arm below
        // consumes it.
        let populations = durable_block_refcounts(&meta, vol_tag, &blocks).await?;
        for (slot, idx) in blocks.iter().copied().enumerate() {
            let offset = idx.saturating_mul(chunk);
            let key = backend.persist_block_key(&be_id, offset);
            let verdict = match alloc.refcount(offset) {
                // Finding 23's SHIELD: a legitimate shipped free follows
                // its displacing publish (the durable ordering point), so
                // the shipper's reference is already released — the
                // durable population runs BELOW the RAM refcount. A verb
                // whose named offset has every RAM reference durably
                // justified (`population ≥ refcount`) names a DEAD
                // lifetime of a freed-and-REALLOCATED offset (the wire
                // carries indices, no incarnation witness): executing it
                // would free the live successor's block. Refuse —
                // leak-safe, counted. Finding 24's arm of the same shield:
                // a tracked offset whose incarnation word is UNSTABLE is
                // CLAIMED and mid-write, so its live successor has no
                // ledger reference yet and the population guard cannot
                // see it; no legitimate displaced free names an unstable
                // offset (the displaced block's last event was its own
                // publish).
                Some(n) if n > 0 && alloc.fill_incarnation(offset).is_none() => {
                    log::error!(
                        "shipped free of block {idx} (vol_tag {vol_tag:#016x}) names an offset \
                         whose incarnation word is UNSTABLE (claimed/mid-write by its current \
                         owner, {n} RAM reference(s)) — a stale duplicate of a dead lifetime; \
                         refused (block_live_free_refusals)"
                    );
                    METRICS
                        .block_live_free_refusals
                        .fetch_add(1, Ordering::Relaxed);
                    FreeVerdict::Refused
                }
                Some(n) if n > 0 && populations[slot] >= n as usize => {
                    log::error!(
                        "shipped free of block {idx} (vol_tag {vol_tag:#016x}) names an offset \
                         whose {n} RAM reference(s) the durable ledger still justifies \
                         (population {}) — a stale duplicate of a dead lifetime; refused \
                         (block_live_free_refusals)",
                        populations[slot]
                    );
                    METRICS
                        .block_live_free_refusals
                        .fetch_add(1, Ordering::Relaxed);
                    FreeVerdict::Refused
                }
                Some(n) if n > 0 => {
                    backend.free_block(&key).await?;
                    if n == 1 {
                        FreeVerdict::Freed
                    } else {
                        FreeVerdict::NonTerminal
                    }
                }
                Some(_) => {
                    // A zero-count entry is the transient window of a
                    // racing terminal release: the OTHER free won, ours is
                    // the double-release lineage. Refuse without poking a
                    // mid-transition entry.
                    log::error!(
                        "shipped free of block {idx} (vol_tag {vol_tag:#016x}) raced a terminal \
                         release mid-transition — refused (double-release lineage)"
                    );
                    METRICS
                        .block_untracked_free_refusals
                        .fetch_add(1, Ordering::Relaxed);
                    FreeVerdict::Refused
                }
                None => {
                    // PR 12b: on a grant-armed allocator the bitmap IS the
                    // free list (PR 8) — the local list is drained into it,
                    // so `free_list_contains` cannot see a joiner-minted
                    // block's earlier release; its bit, CLEAR, can.
                    let bit_clear = alloc
                        .block_grant_vol_tag()
                        .and_then(crate::meta_backend::kv::alloc_lease::holding)
                        .is_some_and(|h| !h.bitmap.is_set(idx));
                    let already = if alloc.free_list_contains(idx) {
                        Some("already on the free list")
                    } else if bit_clear {
                        Some("already CLEAR in the allocation bitmap (the holder's free list)")
                    } else if alloc.grace_holds(offset) {
                        Some("held in the freed-offset grace ring")
                    } else if alloc.is_quarantined(offset) {
                        Some("quarantined under a dead custody epoch")
                    } else if alloc.inflight_contains(offset) {
                        Some("in flight (mid-reclaim or a live owner's unpublished mint)")
                    } else {
                        None
                    };
                    if populations[slot] > 0 {
                        FreeVerdict::NonTerminal
                    } else if let Some(why) = already {
                        // Already free (or owed to grace / a dead epoch /
                        // the reclaimer): the double-release lineage. The
                        // UNSEEDED ladder refuses it on the existing
                        // untracked tripwire — never a second free. The
                        // class is named HERE (the tripwire's own line
                        // cannot tell them apart): a refusal storm of one
                        // class is a duplicate free ISSUER on the shipper.
                        log::error!(
                            "shipped free of block {idx} (vol_tag {vol_tag:#016x}) names an \
                             offset this holder tracks no reference for and that is {why} — \
                             the double-release lineage; refused (block_untracked_free_refusals)"
                        );
                        backend.free_block(&key).await?;
                        FreeVerdict::Refused
                    } else {
                        alloc.seed_shipped_free_reference(offset);
                        backend.free_block(&key).await?;
                        FreeVerdict::Freed
                    }
                }
            };
            verdicts.push(verdict);
        }
        Ok(verdicts)
    })
    .await
}

/// A [`crate::meta_ship::publish::FreeExecutor`] over this holder's data
/// router + metadata set — what `multi_writer::arm_authority_planes`
/// installs on every armed writer (and the rigs install directly).
pub fn router_free_executor(
    backend: Arc<crate::routing::BackendRouter>,
    meta: Arc<RoutedMetaBackend>,
) -> crate::meta_ship::publish::FreeExecutor {
    Arc::new(move |vol_tag: u64, blocks: Vec<u64>| {
        let backend = Arc::clone(&backend);
        let meta = Arc::clone(&meta);
        Box::pin(async move { execute_shipped_frees(&backend, &meta, vol_tag, &blocks).await })
    })
}

/// The durable reference populations of a batch of blocks across the
/// metadata set — `refcount(block) == records under the (vol_tag,
/// block_idx) prefix` (`TREE_BLOCK_REFS`'s own law) — over the trees this
/// mount WRITES (PR 13: a slot tree a joiner leases is a projection here —
/// the joiner's free carries its own tree's verdict).
pub async fn durable_block_refcounts(
    meta: &Arc<RoutedMetaBackend>,
    vol_tag: u64,
    block_idxs: &[u64],
) -> Result<Vec<usize>> {
    let mut out = vec![0usize; block_idxs.len()];
    for kv in meta.volumes.iter() {
        for (slot, idx) in block_idxs.iter().enumerate() {
            out[slot] += kv
                .block_ref_count_maintained(vol_tag, *idx)
                .await
                .map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "durable block-reference count failed on {} while serving a shipped \
                         free: {e}",
                        kv.device_path().display()
                    ))
                })?;
        }
    }
    Ok(out)
}

/// The ledger census of one block — this mount's own trees
/// ([`durable_block_refcounts`] for one index).
pub async fn durable_block_refcount(
    meta: &Arc<RoutedMetaBackend>,
    vol_tag: u64,
    block_idx: u64,
) -> Result<usize> {
    Ok(durable_block_refcounts(meta, vol_tag, &[block_idx]).await?[0])
}

/// The `shipped_free` stats-inode object: 0 on every solo mount (nothing
/// ships where this mount holds every allocation lease).
pub fn stats_json() -> serde_json::Value {
    serde_json::json!({
        "accounting_refusals": METRICS.accounting_plane_refusals.load(Ordering::Relaxed),
        "unpublished_abandons": METRICS.unpublished_mint_abandons.load(Ordering::Relaxed),
        "recomputed_retires": METRICS.recomputed_retires.load(Ordering::Relaxed),
    })
}
