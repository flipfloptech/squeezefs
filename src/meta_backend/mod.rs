pub mod atomicity;
pub mod crossvol_tx;
pub mod dir_stripe;
pub mod dlm;
pub mod kv;
pub mod reservation;
pub mod slot_gate_core;
pub mod slot_migration;
pub mod sync_coalescer;

use crate::error::Result;

pub type Ino = u64;

// ---------------------------------------------------------------------------
// PR VL5b — guest keyspaces (design-volume-lifecycle §5.5.1/§5.5.2, KD-7).
//
// DEVIATION NOTE (verified against the code, documented in the design's
// spirit of §5.5.1): the design sketches guest slots as namespaced TREE
// IDS in the root ledger. The journal record tag byte reserves only a
// NIBBLE for the tree id (`kv::journal::tag_for` — low nibble tree 1..=5,
// high nibble interior level), so per-slot tree ids (up to 64 slots × 3
// trees) would be a journal wire-format change with program-wide blast
// radius. Guest keyspaces are therefore implemented as a **per-slot ino-
// namespace partition inside the existing three trees**: guest slot `s`'s
// records carry local inos in `[(s+1) << 40, (s+2) << 40)`, disjoint from
// the native watermark space (dense from 2) and from every other slot.
// Same isolation, same §4.10 crash contract (guest records are ordinary
// tree records), non-participating volumes byte-identical; the per-slot
// ino cursors ride the A/B root ledger exactly as designed
// (`kv::checkpoint::MembershipStamp::slot_cursors`).
// ---------------------------------------------------------------------------

/// Bits of native local-ino space per volume (≥ 2^40 ≈ 1.1 × 10^12 inos —
/// an order of magnitude past the design's ≥ 100 M-inode cap).
pub const GUEST_NS_SHIFT: u32 = 40;
/// First guest-namespaced local ino; native locals live strictly below.
pub const GUEST_NS_BASE: u64 = 1 << GUEST_NS_SHIFT;

/// The DERIVED virtual routing width (docs/design-dynamic-meta-routing.md
/// §5.1; user ruling 2026-08-02 — widths are derived, never chosen):
/// the FULL slot-id namespace the guest-keyspace wire encoding already
/// reserves (slot ids are u16 in [`guest_local_ino`], the membership
/// stamp, and the cutover gate map). Every `squeezefs format` freezes
/// this value into its stamps; mounts route over the STORED width, so a
/// future derivation change can never re-route an existing set. Growth
/// ceiling = 65536 meta volumes (each member hosts ≥ 1 slot) —
/// unreachable by ≥ 100× at any contemplated deployment.
pub const DERIVED_ROUTING_WIDTH: u32 = (u16::MAX as u32) + 1;

/// Per-volume mint-set size (design-dynamic-meta-routing §5.4): minting
/// rotates across the volume's first `MINT_SPREAD` hosted slots, which
/// is what makes the derived width REAL migration granularity (1/64 of
/// a volume's load per slot) instead of a cosmetic constant — a single
/// mint slot per volume would put every record in V of 65536 slots.
/// Derived from the stamp cursor budget
/// ([`kv::checkpoint::STAMP_MAX_CURSORS`] / 4): a freshly-spread volume
/// consumes ≤ ¼ of its cursor budget, leaving ¾ for cursors travelling
/// in with migrated slots.
pub const MINT_SPREAD: usize = kv::checkpoint::STAMP_MAX_CURSORS / 4;

/// The guest-namespace key ino of guest slot `slot`'s raw local `local`.
pub fn guest_local_ino(slot: u16, local: Ino) -> Ino {
    debug_assert!(
        local < GUEST_NS_BASE,
        "raw local {local:#x} overflows the namespace"
    );
    ((u64::from(slot) + 1) << GUEST_NS_SHIFT) | local
}

/// Split an effective local ino back into `(slot, raw local)`; `None`
/// for native (un-namespaced) locals.
pub fn split_guest_local(local: Ino) -> Option<(u16, Ino)> {
    if local < GUEST_NS_BASE {
        return None;
    }
    let slot = (local >> GUEST_NS_SHIFT) - 1;
    debug_assert!(slot <= u64::from(u16::MAX));
    Some((slot as u16, local & (GUEST_NS_BASE - 1)))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inode {
    pub ino: Ino,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nlink: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub flags: u32,
    /// Device number of char/block device nodes (the kernel's 32-bit
    /// `new_encode_dev` encoding, verbatim from mknod); 0 for every
    /// non-device inode. Persisted in the inode value's `rdev` wire word
    /// (historically the reserved `flags2`).
    pub rdev: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: Ino,
    pub name: String,
    pub file_type: u32,
}

/// What a layout publish GROUP commit answered
/// ([`RoutedMetaBackend::set_layout_and_size_pack_group`]).
pub enum LayoutGroupCommit {
    /// One outcome per item, input order.
    Committed(Vec<Result<()>>),
    /// PK4's single-volume law refused the set after the slot gate: the
    /// members route to `volumes` home meta volumes. Nothing committed.
    Split { volumes: usize },
}

/// One member of a [`RoutedMetaBackend::set_layout_and_size_group`]: the
/// single verb's arguments, owned (the group stages its members
/// concurrently, so each carries its own bytes).
#[derive(Debug)]
pub struct LayoutPublish {
    pub ino: Ino,
    pub layout: Vec<u8>,
    pub size: u64,
    pub block_refs: Vec<kv::block_refs::BlockRefOp>,
}

#[async_trait::async_trait]
pub trait Metadata: Send + Sync {
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode>;
    /// Create with `rdev = 0` — every non-device creation site. Provided:
    /// delegates to [`Metadata::create_with_rdev`].
    async fn create(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Inode> {
        self.create_with_rdev(parent, name, mode, uid, gid, 0).await
    }
    /// `rdev` is the device number for char/block device nodes (mknod's
    /// 32-bit `new_encode_dev` word, persisted in the inode value's
    /// `rdev` wire word); 0 for everything else.
    async fn create_with_rdev(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
    ) -> Result<Inode>;
    async fn unlink(&self, parent: Ino, name: &str) -> Result<Ino>;
    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode>;
    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
        flags: u32,
    ) -> Result<()>;
    async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>>;
    /// Fetch the inode's attributes.
    ///
    /// **4a locking (VL8 item 6):** the [`RoutedMetaBackend`] implementation
    /// takes a **shared** 4a DLM lease on `I{ino}` internally. Callers that
    /// already hold the **exclusive** lease on the same stripe must NOT call
    /// this — the internal shared acquisition self-deadlocks against the
    /// caller's exclusive guard. Read the inode through the already-held
    /// guard's path instead (e.g. `read_inode_routed`), as VL6a's fsck does.
    async fn getattr(&self, ino: Ino) -> Result<Inode>;
    async fn setattr(
        &self,
        ino: Ino,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<u64>,
        mtime: Option<u64>,
        ctime: Option<u64>,
    ) -> Result<Inode>;
    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>>;
    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()>;
    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()>;
    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>>;
    async fn destroy_inode(&self, ino: Ino) -> Result<()>;
}

/// Resolve the deferred-flush interval knob: canonical name first, legacy
/// alias second, default 50 ms (design-wal-crash-consistency §4.2 — a
/// `JOURNAL_`-named knob controlling a flusher with no journal is a
/// permanent naming wart; the alias keeps old operator scripts working).
/// The v3 backend reuses it as the journal/checkpoint cadence
/// (design-cow-kv-metadata §4.6; `0` = strict per-commit barriers).
pub(crate) fn resolve_flush_interval_ms() -> u64 {
    let parse = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|val| val.parse::<u64>().ok())
    };
    parse("SQUEEZEFS_META_FLUSH_INTERVAL_MS")
        .or_else(|| parse("SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS"))
        .unwrap_or(50)
}

/// The loud, precise refusal for legacy format-v2 volumes: v2 support was
/// removed entirely (always forward — no backwards compatibility). One
/// message shape shared by every surface that can meet a v2 superblock
/// (mount open, generation derivation) so operators always see the same
/// actionable text.
pub(crate) fn v2_unsupported_error(path: &str) -> crate::error::SqueezefsError {
    crate::error::SqueezefsError::InvalidOperation(format!(
        "Metadata volume {path} is format v2, which is no longer supported (this binary mounts \
         only format v3) — v2 volumes must be reformatted: run `squeezefs format` (destroys the \
         old contents)"
    ))
}

/// Open `path` for mounting, gated by the sector-0 classification
/// (`kv::superblock::classify_volume`): v3 superblocks mount the KV
/// backend (SB → ledger → bitmap → replay); blank volumes fail loud with
/// the actionable "run `squeezefs format`"; **format-v2 superblocks fail
/// loud as no longer supported** (reformat required); foreign magic, torn
/// superblocks, versions above 3, and unknown incompat feature bits fail
/// loud from the gate itself.
pub async fn open_volume_for_mount(
    path: &str,
) -> Result<std::sync::Arc<kv::backend::KvMetaBackend>> {
    open_volume_gated(path, OpenMode::Write).await
}

/// [`open_volume_for_mount`]'s **read-only probe** twin (same version-gate
/// refusals): full bootstrap + RAM replay but no checkpoint task, so
/// nothing is ever written — the bootstrap config read and status paths
/// use it and drop the backend when done.
pub async fn open_volume_probe(path: &str) -> Result<std::sync::Arc<kv::backend::KvMetaBackend>> {
    open_volume_gated(path, OpenMode::Probe).await
}

/// **DLM S5** — [`open_volume_for_mount`]'s **reader** twin (`-o ro` /
/// `--read-only`, pre-RC engineering spec §6.8 item 1): the same version
/// gate, the same bootstrap + RAM replay, but no Layer-A `LOCK_EX`, no
/// claim gate (so no `FreshForeign` refusal), no PR registration, no
/// superblock repair and no checkpoint task. Unlike
/// [`open_volume_probe`] this mount LATCHES read-only, so every mutation
/// refuses loud for the mount's whole life instead of relying on the
/// caller to drop the handle.
pub async fn open_volume_read_only(
    path: &str,
) -> Result<std::sync::Arc<kv::backend::KvMetaBackend>> {
    open_volume_gated(path, OpenMode::ReadOnlyMount).await
}

/// How [`open_volume_gated`] opens a version-gated volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    /// The D0-guarded write mount.
    Write,
    /// A transient read-only probe (no latch, no tasks — the caller drops
    /// it when done).
    Probe,
    /// DLM S5: a read-only MOUNT (latched, serves FUSE for its lifetime).
    ReadOnlyMount,
}

async fn open_volume_gated(
    path: &str,
    mode: OpenMode,
) -> Result<std::sync::Arc<kv::backend::KvMetaBackend>> {
    match kv::superblock::classify_volume(std::path::Path::new(path)).await? {
        kv::superblock::VolumeFormat::Blank => {
            Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "Metadata volume {path} is not formatted (zeroed superblock) — run \
                 `squeezefs format` first"
            )))
        }
        kv::superblock::VolumeFormat::V2Legacy => Err(v2_unsupported_error(path)),
        kv::superblock::VolumeFormat::V3(_) => {
            let p = std::path::Path::new(path);
            Ok(match mode {
                OpenMode::Probe => kv::backend::KvMetaBackend::open_probe(p).await?,
                OpenMode::ReadOnlyMount => kv::backend::KvMetaBackend::open_read_only(p).await?,
                OpenMode::Write => kv::backend::KvMetaBackend::open(p).await?,
            })
        }
    }
}

/// Open a mount's whole metadata volume set **in set order**, each volume
/// through the version gate + the D0 single-writer guard
/// (design-metadata-throughput §5.0: "multi-volume mounts claim every meta
/// volume in the set (volume order)"). A guard refusal or open failure on
/// volume k releases the guards already taken on volumes `0..k` — flocks,
/// `writer_claim` records, and NVMe reservations — via each backend's
/// clean shutdown, then propagates the volume-k error loud.
///
/// Takes `paths` verbatim — set-order canonicalization (the §5.5.1a
/// stamp discovery) is [`open_routed_meta_set`]'s job; production
/// write-mount paths go through that wrapper.
pub async fn open_meta_volume_set(
    paths: &[String],
) -> Result<Vec<std::sync::Arc<kv::backend::KvMetaBackend>>> {
    let mut opened: Vec<std::sync::Arc<kv::backend::KvMetaBackend>> = Vec::new();
    for path in paths {
        match open_volume_for_mount(path).await {
            Ok(be) => opened.push(be),
            Err(e) => {
                for prior in &opened {
                    if let Err(te) = prior.shutdown().await {
                        log::warn!(
                            "releasing guard on {:?} after a failed set open failed too: {te}",
                            prior.device_path()
                        );
                    }
                }
                return Err(e);
            }
        }
    }
    Ok(opened)
}

/// The §5.4a M1 pre-check's test seam (the `TEST_XV_SEAM_AFTER_STEPS`
/// precedent — a suite arms and disarms it per case, so it cannot be an
/// env knob): `true` bypasses the check, which is the ONLY way to
/// exercise the pre-M1 behaviour the repro exists to record. Nothing in
/// the product ever sets it.
static TEST_DISABLE_CROSS_OWNER_PRECHECK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Arm/disarm the M1 seam above — see
/// `tests/pv_partial_open_tests.rs::the_pre_check_absent_shape_is_what_fail_stops_two_volumes`,
/// the negative twin that records why the pre-check exists.
pub fn test_disable_cross_owner_precheck(on: bool) {
    TEST_DISABLE_CROSS_OWNER_PRECHECK.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The **offline-bracket probes** every writable mount runs on the slot-0
/// volume before it serves: `mw_upgrade:` (KD-MW-1 §6.2 mechanism i) and
/// its sibling `owner_assign:` (per-volume claim admission §5.2.1 / sweep
/// row 2, KD-PV-2 — *"the `MW_UPGRADE_MARKER_XATTR` pattern verbatim"*).
///
/// Both mark an offline verb that is mid-act: an interrupted
/// `enable-multi-writer` upgrade, or an interrupted `volume set-owners`
/// assignment. A writable mount refuses while either exists, naming the
/// idempotent re-run; read-only mounts keep serving. A probe that cannot
/// be READ refuses too — never guess about a bracket.
///
/// `None` = clear, which is every set that is not mid-verb.
async fn open_intent_marker_refusal(
    be: &std::sync::Arc<kv::backend::KvMetaBackend>,
) -> Option<crate::error::SqueezefsError> {
    let refuse = |m: String| Some(crate::error::SqueezefsError::InvalidOperation(m));
    match be.getxattr(1, crate::MW_UPGRADE_MARKER_XATTR).await {
        Ok(Some(raw)) => {
            let named = match crate::MwUpgradeMarker::decode(&raw) {
                Ok(m) => format!("covering volumes {:?}", m.volumes),
                Err(e) => format!("(marker undecodable: {e})"),
            };
            return refuse(format!(
                "refusing a writable mount: a multi-writer upgrade-intent marker \
                 (`{}`) is present on volume 0 {named} — a `squeezefs volume \
                 enable-multi-writer` run crashed mid-upgrade. Re-run `squeezefs \
                 volume enable-multi-writer <sqmeta-uri>` (idempotent, resumes \
                 from the crash point); read-only mounts keep serving",
                crate::MW_UPGRADE_MARKER_XATTR
            ));
        }
        Ok(None) => {}
        Err(e) => {
            return refuse(format!(
                "refusing a writable mount: the multi-writer upgrade-intent marker \
                 probe on volume 0 failed ({e})"
            ));
        }
    }
    match be.getxattr(1, crate::OWNER_ASSIGN_MARKER_XATTR).await {
        Ok(Some(raw)) => {
            let named = match crate::config_ops::OwnerAssignMarker::decode(&raw) {
                Ok(m) => format!(
                    "covering volumes {:?}",
                    m.assignments
                        .iter()
                        .map(|a| a.volume_id.as_str())
                        .collect::<Vec<_>>()
                ),
                Err(e) => format!("(marker undecodable: {e})"),
            };
            refuse(format!(
                "refusing a writable mount: a per-volume ownership-assignment intent marker \
                 (`{}`) is present on the slot-0 volume {named} — a `squeezefs volume \
                 set-owners` run crashed mid-assignment, so some volumes may name an owner \
                 and others may not. Re-run `squeezefs volume set-owners <sqmeta-uri> …` \
                 (idempotent, resumes from the crash point) or `--clear` it; read-only \
                 mounts keep serving",
                crate::OWNER_ASSIGN_MARKER_XATTR
            ))
        }
        Ok(None) => None,
        Err(e) => refuse(format!(
            "refusing a writable mount: the ownership-assignment intent marker probe on the \
             slot-0 volume failed ({e})"
        )),
    }
}

/// **Per-volume claim admission — open a set this mount appends to only
/// PART of** (`docs/design-per-volume-claim-admission.md` §5.4, PR 4):
/// [`open_meta_volume_set`]'s partial twin, one mode per volume.
///
/// `vol_ids` are the DURABLE `vol-{hex}` identities of `ordered` in the
/// same order (KD-5 — the mode is resolved by identity, never by
/// position: an index-keyed vector plus a permuted URI list would take
/// the full D0 ladder on a volume a peer owns, the worst outcome in the
/// program, reached by an off-by-permutation rather than a race).
///
/// The `admission` is carried rather than a `&[VolumeMode]` (the design's
/// §6.3 sketch) for one reason: [`kv::backend::KvMetaBackend::open_peer_owned`]
/// re-checks the decision itself, and `VolumeMode` is a plain public enum
/// any caller could build — passing modes alone would make the peer door
/// reachable with a fabricated mode vector, which is exactly what
/// `SetAdmission`'s private fields exist to prevent.
///
/// **The rollback ladder releases exactly what it took.** An `Own` volume
/// holds a flock, a `writer_claim` and (on a PR namespace) a reservation,
/// released by its `shutdown()`; a `Peer` volume holds none of the three,
/// so its shutdown is a no-op — correct, and pinned in both directions.
pub async fn open_meta_volume_set_partial(
    ordered: &[String],
    vol_ids: &[String],
    admission: &crate::partial_authority::SetAdmission,
) -> Result<Vec<std::sync::Arc<kv::backend::KvMetaBackend>>> {
    if ordered.len() != vol_ids.len() {
        return Err(crate::error::SqueezefsError::InvalidOperation(format!(
            "partial set open: {} volume paths against {} durable ids — the mode of a volume \
             is keyed on its identity, so a mismatched pairing cannot be interpreted",
            ordered.len(),
            vol_ids.len()
        )));
    }
    let mut opened: Vec<std::sync::Arc<kv::backend::KvMetaBackend>> = Vec::new();
    for (path, vol_id) in ordered.iter().zip(vol_ids) {
        let out = match admission.mode_for(vol_id) {
            Some(crate::partial_authority::VolumeMode::Own) => {
                open_volume_for_mount(path).await.map_err(|e| {
                    // The own-mode arm is the shipped D0 ladder, verbatim.
                    e
                })
            }
            Some(crate::partial_authority::VolumeMode::Peer { .. }) => {
                kv::backend::KvMetaBackend::open_peer_owned(
                    std::path::Path::new(path),
                    admission,
                    vol_id,
                )
                .await
                .map_err(Into::into)
            }
            None => Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "partial set open: the admission does not name metadata volume {path} \
                 ({vol_id}). A volume a decision does not cover is a refusal, never a \
                 default — re-run `squeezefs volume set-owners` over the WHOLE set"
            ))),
        };
        match out {
            Ok(be) => opened.push(be),
            Err(e) => {
                for prior in &opened {
                    if let Err(te) = prior.shutdown().await {
                        log::warn!(
                            "releasing guard on {:?} after a failed partial set open failed \
                             too: {te}",
                            prior.device_path()
                        );
                    }
                }
                return Err(e);
            }
        }
    }
    Ok(opened)
}

/// Open a whole metadata volume set as the routed backend (PR VL5a): the
/// §5.5.1a stamp discovery first (canonical `member_position` ordering,
/// loud disagreement refusals), then the guarded [`open_meta_volume_set`]
/// over the CANONICAL order, then a [`RoutedMetaBackend`] carrying the
/// frozen routing width + durable slot map (legacy sets: implicit
/// `W = volume count`, identity map, nothing written).
pub async fn open_routed_meta_set(paths: &[String]) -> Result<std::sync::Arc<RoutedMetaBackend>> {
    let disc = discover_meta_set(paths).await?;
    // Structural validation BEFORE any guard is taken, so a bad map can
    // never leave claims behind (discovery already guarantees this shape;
    // belt and suspenders).
    validate_slot_map(
        disc.ordered_paths.len(),
        disc.routing_width,
        &disc.slot_to_volume,
    )?;
    // KD-MW-1 (design-full-multi-writer §6.2 mechanism ii): the bit-11
    // uniformity half of the writable-mount refusal predicate, checked
    // BEFORE any guard is taken (superblock reads only). The marker half
    // needs volume 0's xattr tree and runs after the guarded opens below.
    refuse_mixed_multi_writer_set(&disc.ordered_paths).await?;
    let backends = open_meta_volume_set(&disc.ordered_paths).await?;
    // §6.2 mechanism i: a writable mount refuses while the `mw_upgrade:`
    // intent marker exists (covers shape (b) on ANY volume — the marker
    // precedes any bit write). The `volume enable-multi-writer` verb's own
    // D0-guarded per-volume opens are the ONE marker-tolerant writable
    // open; they never route through this gate by construction.
    let refusal = open_intent_marker_refusal(&backends[0]).await;
    if let Some(err) = refusal {
        for be in &backends {
            if let Err(te) = be.shutdown().await {
                log::warn!(
                    "releasing guard on {:?} after the mw_upgrade marker refusal failed: {te}",
                    be.device_path()
                );
            }
        }
        return Err(err);
    }
    let routed = std::sync::Arc::new(RoutedMetaBackend::with_slot_map_and_natives(
        backends,
        disc.routing_width,
        disc.slot_to_volume,
        disc.native_slots,
    )?);
    // **DLM S3.5 (design-cow-kv-metadata §4.10a)**: roll every open
    // cross-volume intent forward BEFORE this set serves anything. Each
    // volume's journal replay already ran inside its open, so the intents
    // are RAM-authoritative here; the scan is one bounded range per volume
    // and finds nothing on a healthy set. A failure to recover refuses the
    // open loud rather than serving a half-applied transaction (DUR-7).
    crossvol_tx::recover_open_intents(&routed).await?;
    // The recovery hook is bring-up too: its roll-forward + retirement
    // commits are exactly the committed-but-uncovered residue class the
    // per-volume opens just closed for the claim tx (the D1.b
    // wedge-crumb — `KvMetaBackend::cover_bring_up_residue`). Cover it
    // per volume before the set serves; a no-op read on every volume
    // recovery did not touch.
    for vol in &routed.volumes {
        vol.cover_bring_up_residue().await?;
    }
    // Symmetric PR 6 (§5.6): an armed set's cross-owner intents whose
    // holders were unreachable — at this open's own recovery (the
    // shipper and the holders' endpoints are the join ladder's, bound
    // after it) or at a live op — are rolled forward by the set's own
    // cadence, at the checkpoint landing ceiling. Unarmed: no task.
    if routed.volumes.iter().any(|v| v.slot_lease_armed()) {
        crossvol_tx::spawn_roll_forward_cadence(
            std::sync::Arc::downgrade(&routed),
            kv::checkpoint::checkpoint_landing_ceiling_derived(),
        );
        // PR 7b: the striped directories' background flips and
        // migrations hold the set through this handle.
        routed.install_stripe_self();
    }
    // PR 8 (design-symmetric-metadata §5.5 / KD-SYM-15): under the armed
    // plane volume 0's manager is the maintenance coordinator and this
    // mount is a symmetric appender whose `T_self` action is the park.
    kv::alloc_lease::arm_symmetric_roles(&routed);
    Ok(routed)
}

/// **DLM S5** — [`open_routed_meta_set`]'s **reader** twin (`-o ro` /
/// `--read-only`): the same §5.5.1a stamp discovery, the same canonical
/// member ordering and slot-map validation, opened through
/// [`open_volume_read_only`] per volume.
///
/// No guard is taken on any member, so there is nothing to release on a
/// mid-set failure — the whole rollback ladder
/// [`open_meta_volume_set`] needs is structurally absent for a reader.
pub async fn open_routed_meta_set_read_only(
    paths: &[String],
) -> Result<std::sync::Arc<RoutedMetaBackend>> {
    let disc = discover_meta_set(paths).await?;
    validate_slot_map(
        disc.ordered_paths.len(),
        disc.routing_width,
        &disc.slot_to_volume,
    )?;
    let mut vols = Vec::with_capacity(disc.ordered_paths.len());
    for path in &disc.ordered_paths {
        vols.push(open_volume_read_only(path).await?);
    }
    Ok(std::sync::Arc::new(
        RoutedMetaBackend::with_slot_map_and_natives(
            vols,
            disc.routing_width,
            disc.slot_to_volume,
            disc.native_slots,
        )?,
    ))
}

/// **DLM S9** — [`open_routed_meta_set`]'s **co-writer** twin
/// (`SQUEEZEFS_MW_ROLE=co-writer`, past the five-rung admission ladder):
/// the same §5.5.1a stamp discovery, the same canonical member ordering and
/// slot-map validation, opened through [`kv::backend::KvMetaBackend::open_co_writer`] per
/// volume.
///
/// No guard is taken on any member, so — exactly as for a reader — the
/// whole rollback ladder [`open_meta_volume_set`] needs is structurally
/// absent.
///
/// `crossvol_tx::recover_open_intents` is deliberately NOT run: rolling an
/// open cross-volume intent forward is a WRITE, and it belongs to the
/// authority's own open (which already ran it, or will). A co-writer that
/// recovered intents would append to trees it has no authority over.
pub async fn open_routed_meta_set_co_writer(
    paths: &[String],
    admission: &crate::cowriter::CoWriterAdmission,
) -> Result<std::sync::Arc<RoutedMetaBackend>> {
    let disc = discover_meta_set(paths).await?;
    validate_slot_map(
        disc.ordered_paths.len(),
        disc.routing_width,
        &disc.slot_to_volume,
    )?;
    let mut vols = Vec::with_capacity(disc.ordered_paths.len());
    for path in &disc.ordered_paths {
        vols.push(
            kv::backend::KvMetaBackend::open_co_writer(std::path::Path::new(path), admission)
                .await?,
        );
    }
    Ok(std::sync::Arc::new(
        RoutedMetaBackend::with_slot_map_and_natives(
            vols,
            disc.routing_width,
            disc.slot_to_volume,
            disc.native_slots,
        )?,
    ))
}

/// **Symmetric PR 12b — [`open_routed_meta_set`]'s JOINED-APPENDER twin**
/// (design-symmetric-metadata §7.3, the join ladder's terminal step for a
/// non-manager RW mount of an ARMED set): the same §5.5.1a stamp
/// discovery, the same canonical member ordering and slot-map validation,
/// each volume opened through [`kv::backend::KvMetaBackend::open_joined_appender`]
/// against the manager's published listener — the volume's ordinal is the
/// manager frame's `volume` word, so ONE endpoint serves the whole set.
///
/// No guard is taken on any member (the manager holds them), so a mid-set
/// failure tears down what joined so far through each backend's own
/// shutdown (its slots released over the wire, its page freed) and nothing
/// else. The set then runs the WRITER's tail — its own open cross-owner
/// intents rolled forward (the scan is scoped to the slots this mount's
/// step-home is `Local` for, so two mounts never both adopt one intent —
/// PR 6 round 2), the roll-forward cadence, the striping self-handle and
/// the symmetric roles (a joined appender's `T_self` action is the park,
/// PR 8) — because a joined appender IS a writer for its slots.
pub async fn open_routed_meta_set_joined(
    paths: &[String],
    admission: &JoinedSetAdmission,
) -> Result<std::sync::Arc<RoutedMetaBackend>> {
    let disc = discover_meta_set(paths).await?;
    validate_slot_map(
        disc.ordered_paths.len(),
        disc.routing_width,
        &disc.slot_to_volume,
    )?;
    refuse_mixed_multi_writer_set(&disc.ordered_paths).await?;
    let mut vols: Vec<std::sync::Arc<kv::backend::KvMetaBackend>> =
        Vec::with_capacity(disc.ordered_paths.len());
    // ONE registrant key for the set: every metadata namespace a remote
    // joiner registers on at its door and every data namespace its
    // ladder's rung 4 registers on carry it, so the death ledger's one
    // key preempts this member on every namespace class.
    let registrant_key = loop {
        let k = rand::Rng::gen::<u64>(&mut rand::thread_rng());
        if k != 0 {
            break k;
        }
    };
    for (ordinal, path) in disc.ordered_paths.iter().enumerate() {
        let per_volume = kv::backend::JoinedAppenderAdmission {
            manager_endpoint: admission.manager_endpoint.clone(),
            secret: admission.secret.clone(),
            peer_id: admission.peer_id.clone(),
            volume: u16::try_from(ordinal).map_err(|_| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "joined open: volume ordinal {ordinal} exceeds the manager frame's u16 word"
                ))
            })?,
            identity: admission.identity,
            registrant_key,
        };
        match kv::backend::KvMetaBackend::open_joined_appender(
            std::path::Path::new(path),
            &per_volume,
        )
        .await
        {
            Ok(be) => vols.push(be),
            Err(e) => {
                for prior in &vols {
                    if let Err(te) = prior.shutdown().await {
                        log::warn!(
                            "leaving joined appender region on {:?} after a failed set join \
                             failed too: {te}",
                            prior.device_path()
                        );
                    }
                }
                return Err(e.into());
            }
        }
    }
    let routed = std::sync::Arc::new(RoutedMetaBackend::with_slot_map_and_natives(
        vols,
        disc.routing_width,
        disc.slot_to_volume,
        disc.native_slots,
    )?);
    crossvol_tx::recover_open_intents(&routed).await?;
    for vol in &routed.volumes {
        vol.cover_bring_up_residue().await?;
    }
    crossvol_tx::spawn_roll_forward_cadence(
        std::sync::Arc::downgrade(&routed),
        kv::checkpoint::checkpoint_landing_ceiling_derived(),
    );
    routed.install_stripe_self();
    kv::alloc_lease::arm_symmetric_roles(&routed);
    install_joined_slot_carriage_sink(&routed);
    Ok(routed)
}

/// The member side of the slot-lease carriage for a JOINED set (PR 12b —
/// `membership::install_slot_carriage_sink`): every renewal grant's
/// `slot_release_notices` run flush-then-transfer on the volume hosting
/// each routing slot, every `offered_slots` entry is accepted there —
/// spawned on the meta lanes, never inline in the renewal (a release
/// cycles the checkpoint). The routed set is held weakly: a set that left
/// makes the sink inert.
fn install_joined_slot_carriage_sink(routed: &std::sync::Arc<RoutedMetaBackend>) {
    let weak = std::sync::Arc::downgrade(routed);
    crate::membership::install_slot_carriage_sink(std::sync::Arc::new(
        move |release: &[u16], offered: &[(u16, u32)]| {
            let Some(routed) = weak.upgrade() else {
                return;
            };
            let release = release.to_vec();
            let offered = offered.to_vec();
            crate::meta_exec::spawn_meta_contained("sym_joined_slot_carriage", async move {
                routed.act_on_slot_carriage(&release, &offered).await;
            });
        },
    ));
}

impl RoutedMetaBackend {
    /// Route a grant's slot words to the volumes hosting them and act on
    /// each (`joined_act_on_release_notices` / `joined_accept_offers`).
    /// Returns `(released, accepted)`.
    pub async fn act_on_slot_carriage(
        &self,
        release: &[u16],
        offered: &[(u16, u32)],
    ) -> (usize, usize) {
        let mut released = 0;
        let mut accepted = 0;
        for (v, vol) in self.volumes.iter().enumerate() {
            let mine_release: Vec<u16> = release
                .iter()
                .copied()
                .filter(|s| self.slot_volume(*s) == Some(v))
                .collect();
            let mine_offered: Vec<(u16, u32)> = offered
                .iter()
                .copied()
                .filter(|(s, _)| self.slot_volume(*s) == Some(v))
                .collect();
            if !mine_release.is_empty() {
                released += vol.joined_act_on_release_notices(&mine_release).await;
            }
            if !mine_offered.is_empty() {
                accepted += vol.joined_accept_offers(&mine_offered).await;
            }
        }
        (released, accepted)
    }
}

/// What a non-manager RW mount of an armed set needs to join it — resolved
/// by [`symmetric_join_target`] off DURABLE state (the manager's page 0
/// identity → its claim-set entry's published listener; the set's cluster
/// secret; this node's member id and appender identity).
#[derive(Debug, Clone)]
pub struct JoinedSetAdmission {
    pub manager_endpoint: String,
    pub secret: Vec<u8>,
    pub peer_id: String,
    pub identity: kv::appender::AppenderIdentity,
}

/// **The join ladder's terminal decision** (design-symmetric-metadata
/// §7.3, PR 12b): under `SQUEEZEFS_SYMMETRIC_META=1`, does this set already
/// have a live MANAGER this mount should JOIN rather than claim? Read off
/// the first volume through a probe (nothing written, no lock): a LOCAL
/// exclusive holder of the writer lock (a same-host manager) or a
/// heartbeat-FRESH `writer_claim` of another writer (the D0 gate's own
/// `FreshForeign` class) means yes — then the manager's endpoint is its
/// page 0's identity resolved through the claim set (`sym_join::
/// resolve_holder_endpoint`) and the secret the set's `job:enroll` record.
/// `Ok(None)` = no live manager (this mount walks the D0 ladder and
/// becomes it), or the plane is not requested (the shipped posture
/// exactly). A live manager whose endpoint is unresolvable REFUSES loud:
/// a second RW mount that can neither claim nor join must not half-join.
pub async fn symmetric_join_target(paths: &[String]) -> Result<Option<JoinedSetAdmission>> {
    if !kv::slot_lease::symmetric_meta_requested() {
        return Ok(None);
    }
    let disc = discover_meta_set(paths).await?;
    let Some(first) = disc.ordered_paths.first() else {
        return Ok(None);
    };
    let probe = open_volume_probe(first).await?;
    if !probe.superblock().symmetric_forest_stamped() {
        return Ok(None);
    }
    let live_manager = matches!(
        kv::backend::KvMetaBackend::probe_shared_lock(std::path::Path::new(first)),
        kv::backend::SharedProbe::LocalExclusiveHolder
    ) || matches!(
        probe.claim_standing().await,
        crate::partial_authority::ClaimStanding::Fresh
    );
    if !live_manager {
        return Ok(None);
    }
    let endpoint = crate::sym_join::resolve_holder_endpoint(&probe, 0).await;
    let secret = crate::membership::cluster_secret(&probe).await;
    let peer_id = crate::cowriter::node_member_id()?;
    drop(probe);
    let (Some(endpoint), Some(secret)) = (endpoint, secret) else {
        return Err(crate::error::SqueezefsError::InvalidOperation(format!(
            "refusing to mount: metadata volume {first} has a LIVE manager but this mount cannot \
             JOIN it — the manager's published listener or the set's cluster secret is missing \
             (the manager's join ladder publishes its endpoint at rung 7; the secret is the \
             `job:enroll` record). A second RW mount of an armed set joins as a writer or refuses; \
             it never half-joins (design-symmetric-metadata §7.3, PR 12b)"
        )));
    };
    // The same `(node, mount slot)` scope the manager's open binds its
    // pages to — ONE derivation (`appender_identity_scope`).
    let (node_token, mount_slot) =
        kv::backend::KvMetaBackend::appender_identity_scope(&kv::backend::read_boot_id());
    Ok(Some(JoinedSetAdmission {
        manager_endpoint: endpoint,
        secret,
        peer_id,
        identity: kv::appender::AppenderIdentity {
            node_token,
            mount_slot,
            writer_id: 0,
        },
    }))
}

/// **Per-volume claim admission — [`open_routed_meta_set`]'s PARTIAL twin
/// and the mount path's entry point** (§5.4 sweep row 18, PR 4), modeled
/// on [`open_routed_meta_set_co_writer`]:
///
/// discovery → canonical order → resolve each volume's mode by its
/// DURABLE id → the partial set open (rollback ladder included) → the two
/// offline-bracket probes → **ownership-scoped** intent recovery → the
/// bring-up cover on OWNED volumes only → the KD-PV-17 holder attestation
/// → `RoutedMetaBackend`.
///
/// Four of those differ from the write twin, each because a peer's volume
/// is not this mount's to touch:
///
/// * **row 3** — `recover_open_intents` becomes
///   [`crossvol_tx::recover_open_intents_scoped`]: roll forward what we
///   own, skip what a peer owns, refuse loud on an intent spanning two
///   owners;
/// * **row 4** — `cover_bring_up_residue` WRITES, so it runs on owned
///   volumes only;
/// * **row 2** — the `owner_assign:` probe joins `mw_upgrade:`;
/// * KD-PV-17 — each owned volume's claim gets its holder attestation, the
///   durable fact that lets a PEER resolve this mount's per-mount claim
///   uuid to its enrollment identity.
pub async fn open_routed_meta_set_partial(
    paths: &[String],
    admission: &crate::partial_authority::SetAdmission,
) -> Result<std::sync::Arc<RoutedMetaBackend>> {
    let disc = discover_meta_set(paths).await?;
    validate_slot_map(
        disc.ordered_paths.len(),
        disc.routing_width,
        &disc.slot_to_volume,
    )?;
    refuse_mixed_multi_writer_set(&disc.ordered_paths).await?;
    if !admission.covers(&disc.ordered_paths) {
        return Err(crate::error::SqueezefsError::InvalidOperation(format!(
            "refusing a partial-writer mount: the admission was decided over {:?}, but this \
             set's canonical membership is {:?}. An admission is per-SET — bit 14 and a \
             durable claim set on every volume, one complete assignment map, one set \
             authority — so it may never be carried across sets",
            admission.volumes(),
            disc.ordered_paths
        )));
    }
    // KD-5, and the reason discovery runs FIRST: modes are keyed on the
    // durable identity, resolved against the CANONICAL order rather than
    // the caller's URI order.
    let vol_ids: Vec<String> = disc
        .uuids
        .iter()
        .map(kv::backend::durable_volume_id_of)
        .collect();
    let backends = open_meta_volume_set_partial(&disc.ordered_paths, &vol_ids, admission).await?;
    let owned: Vec<bool> = vol_ids
        .iter()
        .map(|id| admission.mode_for(id) == Some(&crate::partial_authority::VolumeMode::Own))
        .collect();

    if let Some(err) = open_intent_marker_refusal(&backends[0]).await {
        for be in &backends {
            if let Err(te) = be.shutdown().await {
                log::warn!(
                    "releasing guard on {:?} after an intent-marker refusal failed: {te}",
                    be.device_path()
                );
            }
        }
        return Err(err);
    }
    let routed = std::sync::Arc::new(RoutedMetaBackend::with_slot_map_and_natives(
        backends,
        disc.routing_width,
        disc.slot_to_volume,
        disc.native_slots,
    )?);
    let mut bring_up: Result<()> = crossvol_tx::recover_open_intents_scoped(&routed, &owned)
        .await
        .map(|_| ());
    if bring_up.is_ok() {
        for (vol, mine) in routed.volumes.iter().zip(&owned) {
            if *mine {
                bring_up = vol.cover_bring_up_residue().await.map_err(Into::into);
                if bring_up.is_err() {
                    break;
                }
            }
        }
    }
    if let Err(e) = bring_up {
        for be in &routed.volumes {
            if let Err(te) = be.shutdown().await {
                log::warn!(
                    "releasing guard on {:?} after a partial bring-up failure: {te}",
                    be.device_path()
                );
            }
        }
        return Err(e);
    }
    // KD-PV-17, last: the attestation names a claim this mount now holds,
    // so it can only be written after the D0 ladder committed it — and
    // only on volumes we own, where we are the appender.
    for (vol, mine) in routed.volumes.iter().zip(&owned) {
        if !*mine {
            continue;
        }
        if let Err(e) = crate::membership::publish_claim_holder(
            vol,
            admission.node_id(),
            crate::dlm::durable_term(),
        )
        .await
        {
            log::warn!(
                "meta volume {}: publishing this mount's holder attestation failed ({e}) — \
                 peers will read an unresolvable holder for this volume and REFUSE it \
                 (KD-PV-17: silence never adopts), so this mount serves but the fleet cannot \
                 grow until it succeeds",
                vol.device_path().display()
            );
        }
    }
    Ok(routed)
}

/// [`open_routed_meta_set`]'s **read-only probe** twin (the clients/df/
/// job-record access pattern — no D0 claims, no checkpoint tasks,
/// nothing written): same §5.5.1a discovery + canonical ordering, probe
/// opens per volume.
pub async fn open_probe_routed_meta_set(
    paths: &[String],
) -> Result<std::sync::Arc<RoutedMetaBackend>> {
    let disc = discover_meta_set(paths).await?;
    let mut vols = Vec::with_capacity(disc.ordered_paths.len());
    for path in &disc.ordered_paths {
        vols.push(open_volume_probe(path).await?);
    }
    Ok(std::sync::Arc::new(
        RoutedMetaBackend::with_slot_map_and_natives(
            vols,
            disc.routing_width,
            disc.slot_to_volume,
            disc.native_slots,
        )?,
    ))
}

/// Pure frozen-width ino routing (PR VL5a, KD-7): global ino →
/// `(slot, local ino)` over the durable `routing_width W` — never over a
/// live volume count, so global inos are eternally stable across set
/// changes. `W ≤ 1` short-circuits to the EXACT identity encoding
/// (`(0, ino)`) the pre-VL5a single-volume code used — every existing
/// single-meta-volume filesystem's st_ino stability rides on it; for
/// W = 1 the general arithmetic coincides anyway
/// (`(ino−2)/1 + 2 = ino`). Ino 1 (the root) pins to slot 0.
pub fn route_ino_width(ino: Ino, width: u64) -> (u64, Ino) {
    if width <= 1 {
        return (0, ino);
    }
    if ino == 1 {
        return (0, 1);
    }
    ((ino - 2) % width, (ino - 2) / width + 2)
}

/// The intent-apply create preset (rung 13, KD-MW-13): the pre-supplied
/// GLOBAL ino + the client's mint instant — see
/// [`RoutedMetaBackend::create_with_rdev_preset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntentCreatePreset {
    /// The child's pre-supplied global ino (from an owner-reserved
    /// [`crate::meta_ship::InoSupply`]).
    pub global_ino: Ino,
    /// The client's mint instant (the record's times).
    pub ts_ns: u64,
}

/// [`route_ino_width`]'s exact inverse: `(local ino, slot)` → global ino
/// over the frozen width. Identity for `W ≤ 1`; local 1 on slot 0 is
/// the root pin.
pub fn make_global_ino_width(local_ino: Ino, slot: u64, width: u64) -> Ino {
    if width <= 1 {
        return local_ino;
    }
    if local_ino == 1 && slot == 0 {
        return 1;
    }
    (local_ino - 2) * width + slot + 2
}

/// Validate a slot map's shape against a volume count + frozen width and
/// derive each volume's **native mint slot** (its smallest hosted slot —
/// where `make_global_ino` places the volume's freshly-allocated locals;
/// in VL5a's identity distribution that is exactly the volume's
/// `member_position`). Shared by [`RoutedMetaBackend::with_slot_map`]
/// and the pre-open check in [`open_routed_meta_set`].
fn validate_slot_map(
    volume_count: usize,
    routing_width: u64,
    slot_to_volume: &[usize],
) -> Result<Vec<u64>> {
    let refuse = |msg: String| {
        Err(crate::error::SqueezefsError::InvalidOperation(format!(
            "metadata slot map invalid: {msg}"
        )))
    };
    if volume_count == 0 {
        return refuse("no metadata volumes".to_string());
    }
    if routing_width < volume_count as u64 {
        return refuse(format!(
            "routing width {routing_width} below the volume count {volume_count}"
        ));
    }
    if slot_to_volume.len() as u64 != routing_width {
        return refuse(format!(
            "map covers {} slots but the frozen routing width is {routing_width}",
            slot_to_volume.len()
        ));
    }
    let mut native: Vec<u64> = vec![u64::MAX; volume_count];
    for (slot, &vol) in slot_to_volume.iter().enumerate() {
        if vol >= volume_count {
            return refuse(format!(
                "slot {slot} maps to volume index {vol} of {volume_count}"
            ));
        }
        if native[vol] == u64::MAX {
            native[vol] = slot as u64;
        }
    }
    if let Some(orphan) = native.iter().position(|&s| s == u64::MAX) {
        return refuse(format!(
            "volume index {orphan} hosts no slot — every member must host at least one \
             (its native mint slot)"
        ));
    }
    Ok(native)
}

/// The routed multi-volume metadata backend: `volumes` stripes inos over
/// per-volume [`kv::backend::KvMetaBackend`]s (format v3 is the only
/// metadata format). `Arc`-shared (not `Clone`): the volumes own live
/// backends (checkpoint tasks, caches) that must not fork.
///
/// PR VL5a (KD-7): routing runs over the FROZEN `routing_width` + the
/// runtime slot→volume lookup table (the durable §5.5.1a slot map's
/// cache, refreshed at open) — never over `volumes.len()` directly.
/// Legacy sets ([`RoutedMetaBackend::new`]) synthesize the implicit
/// `W = volume count` identity map, byte-identical to the pre-VL5a
/// behavior.
pub struct RoutedMetaBackend {
    pub volumes: Vec<std::sync::Arc<kv::backend::KvMetaBackend>>,
    pub disabled_volumes: std::sync::Arc<dashmap::DashMap<usize, bool, ahash::RandomState>>,
    /// The frozen routing width W (KD-7). Legacy: the volume count.
    routing_width: u64,
    /// The SWAPPABLE routing tables (PR VL5b): the slot→volume map and
    /// each volume's mint slot, published atomically by the migration
    /// flip and read latch-free by every `route_ino` (arc-swap — the
    /// slot_gate_core protocol guarantees no admitted mutation straddles
    /// a swap).
    route: arc_swap::ArcSwap<RouteTable>,
    /// Per volume: the slot whose records live in its LEGACY
    /// (un-namespaced) keyspace — fixed for the volume's lifetime
    /// (§5.5.1a stamps' `resolved_native_slot`); `None` = a fresh
    /// `add-meta` member whose every hosted slot is a guest.
    legacy_slot: Vec<Option<u16>>,
    /// PR VL5b (§5.5.2a): the per-slot cutover gates — armed only while
    /// a migration runs (one relaxed load on the mutation path when
    /// idle).
    gates: SlotGates,
    /// PR VL5b: one slot migration at a time per routed set (the
    /// coordinator's one-flip-per-slot law, made structural).
    pub(crate) migration_lock: crate::sqz_sync::SqzMutex<()>,
    /// Per-volume mint rotors (design-dynamic-meta-routing §5.4): a
    /// relaxed counter whose modulo over the mint set picks each fresh
    /// ino's slot. Relaxed is sufficient — the rotor feeds a pure
    /// distribution choice, never an ordering edge.
    mint_rr: Vec<std::sync::atomic::AtomicUsize>,
    /// PR 3 (kvmap, Rev 1.3 #3): the block-key → tree-7 record encoder,
    /// installed by `DataRouter::set_meta_backend` (the volume-census
    /// round-trip law lives on the router — `BackendRouter::
    /// block_key_map_entry`). Uninstalled (bare backends, offline tools,
    /// the shipped-verb owner without a mounted data plane) falls back to
    /// STRING verbatim — the PR 2 form, which every decoder resolves
    /// forever.
    map_entry_encoder: std::sync::OnceLock<MapEntryEncoder>,
    /// PR 5b (kvmap): the encoder's MIRROR — tree-7 record → router-true
    /// block-key string, installed at the same site. The claims-scoped
    /// train's f36b recompute resolves displaced records through it;
    /// uninstalled falls back to STRING-utf8 (the encoder-less arm only
    /// ever stored STRING records). Since PR 6a the hook takes the
    /// per-index DELTA into the record (0 for point-class records; a
    /// run's covered indices resolve through the router's stride census
    /// — design §12).
    map_entry_decoder: std::sync::OnceLock<MapEntryDecoder>,
    /// PR 6a (design §12): the run-emission STRIDE census — `vol_tag` →
    /// the owning data volume's block size in bytes (the one-block offset
    /// stride a RUN record's arithmetic rides). Installed beside the
    /// encoder at `DataRouter::set_meta_backend`; uninstalled = no run
    /// emission (bare backends only ever encode STRING records, which
    /// never coalesce).
    map_run_stride: std::sync::OnceLock<MapRunStride>,
    /// Symmetric PR 6 (review round 1, Issue 10): the DIRECTORY-parent
    /// memo `dir → (parent, name)` the set-wide directory-rename lock's
    /// ancestor walk reads first — one O(1) probe per hop instead of a
    /// whole-set reverse dentry scan while every other directory rename
    /// in the set waits on the lease. A HINT, never the truth: every hop
    /// is confirmed by the exact lookup at the link's holder, a denied
    /// hint is invalidated and the scan runs (`dir_rename_parent_scans`).
    /// Fed by every directory mint and directory rename this mount
    /// performs and by every confirmed hop; sized by the dentry-cache
    /// derivation (one entry per hot directory — the `..` memo's law).
    dir_parents: moka::sync::Cache<u64, (u64, std::sync::Arc<str>), ahash::RandomState>,
    /// Symmetric PR 7b (design §5.6.5): the striped directories' map
    /// cache, the flip trigger's foreign-creator census and the in-flight
    /// flips/migrations — inert on an unarmed mount.
    dir_stripes: dir_stripe::StripeState,
}

/// The directory-parent memo's capacity: the dentry-cache derivation
/// (`mem_budget::dir_entry_capacity` — one entry per hot directory, the
/// same function the FUSE dentry cache sizes by; never a fixed constant).
fn dir_parent_memo() -> moka::sync::Cache<u64, (u64, std::sync::Arc<str>), ahash::RandomState> {
    moka::sync::Cache::builder()
        .max_capacity(crate::mem_budget::dir_entry_capacity(
            crate::mem_budget::shared_system_ram_bytes(),
        ))
        .build_with_hasher(ahash::RandomState::new())
}

/// See [`RoutedMetaBackend::install_map_entry_encoder`].
type MapEntryEncoder = std::sync::Arc<dyn Fn(&str) -> kv::block_map::MapEntry + Send + Sync>;

/// See [`RoutedMetaBackend::install_map_entry_decoder`].
type MapEntryDecoder =
    std::sync::Arc<dyn Fn(&kv::block_map::MapEntry, u32) -> Option<String> + Send + Sync>;

/// See [`RoutedMetaBackend::install_map_run_stride`].
type MapRunStride = std::sync::Arc<dyn Fn(u64) -> Option<u64> + Send + Sync>;

/// The latch-free routing tables `route_ino`/`make_global_ino` read.
struct RouteTable {
    /// `slot → volumes index`.
    slot_to_volume: Vec<usize>,
    /// Per volume: its MINT SET — the first `min(MINT_SPREAD, hosted)`
    /// hosted slots ascending, derived from the map on every publish
    /// (design-dynamic-meta-routing §5.4; no new durable state — the
    /// per-slot cursors are the durable half and already travel at
    /// cutover). [`RoutedMetaBackend::pick_mint_slot`] rotates over it.
    mint_slots: Vec<Vec<u16>>,
}

/// PR 6a (design §12/A6): the post-encode RUN seam — coalesce a sorted
/// `(index, record)` sequence's straight spans into RUN/RUN2 records.
/// `pub` for `meta_lv_bench`'s run-emission row (the seam's per-publish
/// CPU price); production callers stay inside `migrate_block_map_train`.
/// A span extends while indices are consecutive, the volume tag is one,
/// offsets advance by exactly the volume's block stride (`stride_of` —
/// the router's census; an unknown tag never coalesces), and — for
/// stamped keys — the incarnation stamps are LITERALLY consecutive
/// composed words (consecutive-mint lane_seqs prove stride-1; the
/// emitter verifies every ACTUAL stamp, so decode arithmetic reproduces
/// exactly what was minted). STRING records (decorated keys,
/// encoder-less arms) break spans. Runs cap at
/// [`kv::block_map::RUN_LEN_MAX`] (the A6 floor-probe bound) and need
/// length ≥ 2 (a 1-run is a point — one spelling per mapping, or the
/// record-equality diff churns).
pub fn coalesce_map_runs(
    records: Vec<(u32, kv::block_map::MapEntry)>,
    stride_of: &(dyn Fn(u64) -> Option<u64> + Send + Sync),
) -> Vec<(u32, kv::block_map::MapEntry)> {
    use kv::block_map::{MapEntry, RUN_LEN_MAX};
    let mut out: Vec<(u32, MapEntry)> = Vec::with_capacity(records.len());
    // The open span: (start_index, vol_tag, start_offset, stride,
    // start_incarnation — 0 = unstamped span, len).
    struct Span {
        start: u32,
        vol_tag: u64,
        start_offset: u64,
        stride: u64,
        start_inc: u64,
        len: u32,
    }
    let mut open: Option<Span> = None;
    let flush = |out: &mut Vec<(u32, MapEntry)>, span: Option<Span>| {
        let Some(s) = span else { return };
        if s.len >= 2 {
            let entry = if s.start_inc == 0 {
                MapEntry::Run {
                    vol_tag: s.vol_tag,
                    start_offset: s.start_offset,
                    len: s.len,
                }
            } else {
                MapEntry::RunStamped {
                    vol_tag: s.vol_tag,
                    start_offset: s.start_offset,
                    len: s.len,
                    start_incarnation: s.start_inc,
                }
            };
            out.push((s.start, entry));
        } else {
            // A 1-span re-emits the point verbatim.
            let entry = if s.start_inc == 0 {
                MapEntry::Point {
                    vol_tag: s.vol_tag,
                    offset: s.start_offset,
                }
            } else {
                MapEntry::PointStamped {
                    vol_tag: s.vol_tag,
                    offset: s.start_offset,
                    incarnation: s.start_inc,
                }
            };
            out.push((s.start, entry));
        }
    };
    for (idx, entry) in records {
        let (vol_tag, offset, inc) = match &entry {
            MapEntry::Point { vol_tag, offset } => (*vol_tag, *offset, 0u64),
            MapEntry::PointStamped {
                vol_tag,
                offset,
                incarnation,
            } => (*vol_tag, *offset, *incarnation),
            // STRING (and any pre-coalesced run a caller hands back)
            // breaks the span and passes through verbatim.
            _ => {
                flush(&mut out, open.take());
                out.push((idx, entry));
                continue;
            }
        };
        if let Some(s) = &mut open {
            let extends = s.vol_tag == vol_tag
                && s.len < RUN_LEN_MAX
                && idx == s.start.wrapping_add(s.len)
                && idx > s.start
                && offset == s.start_offset.wrapping_add(s.stride * u64::from(s.len))
                && offset > s.start_offset
                && if s.start_inc == 0 {
                    inc == 0
                } else {
                    inc == s.start_inc.wrapping_add(u64::from(s.len)) && inc > s.start_inc
                };
            if extends {
                s.len += 1;
                continue;
            }
            let closed = open.take();
            flush(&mut out, closed);
        }
        match stride_of(vol_tag) {
            Some(stride) if stride > 0 => {
                open = Some(Span {
                    start: idx,
                    vol_tag,
                    start_offset: offset,
                    stride,
                    start_inc: inc,
                    len: 1,
                });
            }
            _ => {
                // No stride census for this tag: the point stands alone.
                out.push((idx, entry));
            }
        }
    }
    flush(&mut out, open.take());
    out
}

/// Derive each volume's mint set from an expanded slot map: the first
/// `min(MINT_SPREAD, hosted)` hosted slots ascending.
fn derive_mint_slots(volume_count: usize, slot_to_volume: &[usize]) -> Vec<Vec<u16>> {
    let mut mint: Vec<Vec<u16>> = vec![Vec::new(); volume_count];
    for (slot, &v) in slot_to_volume.iter().enumerate() {
        if mint[v].len() < MINT_SPREAD {
            mint[v].push(slot as u16);
        }
    }
    mint
}

/// PR VL5b (§5.5.2a): the armed cutover gates. `armed` is the zero-cost
/// fast path (0 = no migration has ever armed a gate on this mount);
/// `notify` wakes parked ops on reopen (register-recheck — no lost
/// wakeups).
struct SlotGates {
    armed: std::sync::atomic::AtomicU64,
    map: scc::HashMap<u64, std::sync::Arc<slot_gate_core::SlotGate>>,
    notify: squeezefs_ipc::sqz_notify::Notify,
}

impl SlotGates {
    fn new() -> Self {
        Self {
            armed: std::sync::atomic::AtomicU64::new(0),
            map: scc::HashMap::new(),
            notify: squeezefs_ipc::sqz_notify::Notify::new(),
        }
    }
}

/// RAII pass through the armed slot gates an op's touched-slot set hit:
/// exits every entered gate at the op's terminal outcome (drop), which
/// is what the cutover's drain waits on.
#[derive(Default)]
pub(crate) struct SlotGatePass {
    entered: Vec<(u64, std::sync::Arc<slot_gate_core::SlotGate>)>,
}

impl Drop for SlotGatePass {
    fn drop(&mut self) {
        for (_, gate) in &self.entered {
            gate.exit();
        }
    }
}

impl RoutedMetaBackend {
    /// The LEGACY constructor: implicit `W = volume count`, identity
    /// slot map — routes exactly as every pre-VL5a set did (nothing
    /// durable exists to read; stamped sets come through
    /// [`Self::with_slot_map`] via [`open_routed_meta_set`]).
    pub fn new(volumes: Vec<std::sync::Arc<kv::backend::KvMetaBackend>>) -> Self {
        let n = volumes.len();
        // DLM S4: lock homing routes over the SAME width the metadata
        // plane does (spec §6.7 decision 2).
        crate::dlm_slot::publish_routing_width(n as u64);
        Self {
            volumes,
            disabled_volumes: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            routing_width: n as u64,
            route: arc_swap::ArcSwap::from_pointee(RouteTable {
                slot_to_volume: (0..n).collect(),
                mint_slots: (0..n).map(|v| vec![v as u16]).collect(),
            }),
            legacy_slot: (0..n).map(|v| Some(v as u16)).collect(),
            gates: SlotGates::new(),
            migration_lock: crate::sqz_sync::SqzMutex::new(()),
            mint_rr: (0..n)
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            map_entry_encoder: std::sync::OnceLock::new(),
            map_entry_decoder: std::sync::OnceLock::new(),
            map_run_stride: std::sync::OnceLock::new(),
            dir_parents: dir_parent_memo(),
            dir_stripes: dir_stripe::StripeState::new(),
        }
    }

    /// Construct over an explicit frozen width + slot map (the §5.5.1a
    /// stamps' reconstruction, in canonical `member_position` order),
    /// with each volume's legacy-keyspace slot derived as its smallest
    /// hosted slot — correct for identity-distributed (never-migrated)
    /// sets; migrated sets come through [`Self::with_slot_map_and_natives`]
    /// via [`open_routed_meta_set`]. Refuses malformed maps loud.
    pub fn with_slot_map(
        volumes: Vec<std::sync::Arc<kv::backend::KvMetaBackend>>,
        routing_width: u64,
        slot_to_volume: Vec<usize>,
    ) -> Result<Self> {
        let native = validate_slot_map(volumes.len(), routing_width, &slot_to_volume)?;
        let legacy = native.iter().map(|&s| Some(s as u16)).collect();
        Self::with_slot_map_and_natives(volumes, routing_width, slot_to_volume, legacy)
    }

    /// [`Self::with_slot_map`] with EXPLICIT per-volume legacy-keyspace
    /// slots (PR VL5b — the §5.5.1a stamps' `resolved_native_slot`,
    /// which the smallest-hosted derivation gets wrong after any slot
    /// migration).
    pub fn with_slot_map_and_natives(
        volumes: Vec<std::sync::Arc<kv::backend::KvMetaBackend>>,
        routing_width: u64,
        slot_to_volume: Vec<usize>,
        legacy_slot: Vec<Option<u16>>,
    ) -> Result<Self> {
        validate_slot_map(volumes.len(), routing_width, &slot_to_volume)?;
        if legacy_slot.len() != volumes.len() {
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "metadata slot map invalid: {} legacy-keyspace slots for {} volumes",
                legacy_slot.len(),
                volumes.len()
            )));
        }
        let n = volumes.len();
        let mint_slots = derive_mint_slots(n, &slot_to_volume);
        // DLM S4: lock homing routes over the SAME frozen width the
        // metadata plane does (spec §6.7 decision 2).
        crate::dlm_slot::publish_routing_width(routing_width);
        Ok(Self {
            volumes,
            disabled_volumes: std::sync::Arc::new(dashmap::DashMap::with_hasher(
                ahash::RandomState::new(),
            )),
            routing_width,
            route: arc_swap::ArcSwap::from_pointee(RouteTable {
                slot_to_volume,
                mint_slots,
            }),
            legacy_slot,
            gates: SlotGates::new(),
            migration_lock: crate::sqz_sync::SqzMutex::new(()),
            mint_rr: (0..n)
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect(),
            map_entry_encoder: std::sync::OnceLock::new(),
            map_entry_decoder: std::sync::OnceLock::new(),
            map_run_stride: std::sync::OnceLock::new(),
            dir_parents: dir_parent_memo(),
            dir_stripes: dir_stripe::StripeState::new(),
        })
    }

    /// PR 3 (kvmap): install the block-key → record encoder (once, at
    /// `DataRouter::set_meta_backend` — the round-trip law needs the
    /// router's volume census). A second install is ignored: the census
    /// it captures is the same router.
    pub fn install_map_entry_encoder(
        &self,
        encoder: std::sync::Arc<dyn Fn(&str) -> kv::block_map::MapEntry + Send + Sync>,
    ) {
        let _ = self.map_entry_encoder.set(encoder);
    }

    /// The record form for one block-key string: POINT through the
    /// installed encoder, STRING verbatim when none is installed (the
    /// PR 2 form — always decodable).
    fn encode_map_entry(&self, key: &str) -> kv::block_map::MapEntry {
        match self.map_entry_encoder.get() {
            Some(enc) => enc(key),
            None => kv::block_map::MapEntry::String(key.as_bytes().to_vec()),
        }
    }

    /// PR 5b (kvmap): install the record → block-key DECODER — the
    /// encoder's mirror (`BackendRouter::map_entry_block_key`), installed
    /// at the same `DataRouter::set_meta_backend` site. The claims-scoped
    /// train's f36b recompute resolves DISPLACED tree records through it.
    pub fn install_map_entry_decoder(
        &self,
        decoder: std::sync::Arc<
            dyn Fn(&kv::block_map::MapEntry, u32) -> Option<String> + Send + Sync,
        >,
    ) {
        let _ = self.map_entry_decoder.set(decoder);
    }

    /// PR 6a (design §12): install the run-emission stride census —
    /// `vol_tag` → the owning data volume's block size. Same install
    /// site and once-wins posture as the encoder/decoder pair.
    pub fn install_map_run_stride(
        &self,
        stride: std::sync::Arc<dyn Fn(u64) -> Option<u64> + Send + Sync>,
    ) {
        let _ = self.map_run_stride.set(stride);
    }

    /// The router-true block-key string for one tree-7 record: through
    /// the installed decoder; STRING utf8 verbatim when none is installed
    /// (the encoder-less arm only ever stored STRING records, so the
    /// fallback is exact); a POINT with no decoder is unresolvable —
    /// `None`, and the caller must be LOUD.
    pub fn decode_map_entry(&self, entry: &kv::block_map::MapEntry) -> Option<String> {
        self.decode_map_entry_at(entry, 0)
    }

    /// [`Self::decode_map_entry`] at a per-index DELTA into a run record
    /// (0 for point-class records — design §12): the claims train's
    /// dissolve/recompute resolve covered indices through it.
    pub fn decode_map_entry_at(
        &self,
        entry: &kv::block_map::MapEntry,
        delta: u32,
    ) -> Option<String> {
        match self.map_entry_decoder.get() {
            Some(dec) => dec(entry, delta),
            None => match entry {
                kv::block_map::MapEntry::String(bytes) if delta == 0 => {
                    std::str::from_utf8(bytes).ok().map(str::to_string)
                }
                _ => None,
            },
        }
    }

    /// PR VL5b: publish a new slot→volume map (the migration flip's
    /// runtime swap — one atomic arc-swap; the §5.5.2a gate protocol
    /// guarantees no admitted mutation straddles it). Refuses malformed
    /// maps loud, leaving the old table serving.
    pub fn publish_slot_map(&self, slot_to_volume: Vec<usize>) -> Result<()> {
        validate_slot_map(self.volumes.len(), self.routing_width, &slot_to_volume)?;
        let mint_slots = derive_mint_slots(self.volumes.len(), &slot_to_volume);
        self.route.store(std::sync::Arc::new(RouteTable {
            slot_to_volume,
            mint_slots,
        }));
        Ok(())
    }

    /// The current slot→volume map (a snapshot copy — control-plane
    /// surface for the migration engine and tests).
    pub fn slot_map_snapshot(&self) -> Vec<usize> {
        self.route.load().slot_to_volume.clone()
    }

    /// The volume whose LEGACY keyspace belongs to `slot` (fixed for
    /// the mount's lifetime), if any.
    pub fn legacy_slot_of(&self, v_idx: usize) -> Option<u16> {
        self.legacy_slot.get(v_idx).copied().flatten()
    }

    /// The frozen routing width W this set routes over (KD-7).
    pub fn routing_width(&self) -> u64 {
        self.routing_width
    }

    /// Routed dentry read: `(stored child ino (global), S_IFMT bits)`.
    async fn find_dentry_routed(
        &self,
        idx: usize,
        local_parent: Ino,
        name: &str,
    ) -> Result<Option<(Ino, u32)>> {
        self.volumes[idx]
            .routed_find_dentry(local_parent, name)
            .await
    }

    /// **§5.4a M1 — the local cross-owner pre-check** (KD-PV-11,
    /// correctness-class, landed unconditionally).
    ///
    /// `route_verb` inspects a call's **named** inos only, and
    /// `MetaCall::named_inos` answers the PARENT ALONE for `Unlink`: the
    /// child and rename's moved/overwritten inodes are DISCOVERED under
    /// guards, so no router can see them. Without this check an ordinary
    /// `rm` of a peer-owned child builds an `XvPlan`, commits step 0 (the
    /// dentry removal, carrying the intent record) on the parent's volume,
    /// and refuses the child's half at the peer-owned write gate —
    /// `escalate_midplan` then fail-stops BOTH volumes and leaves a
    /// durable intent spanning two owners that no process in the fleet can
    /// roll forward. One `rm`, two volumes offline, the next mount
    /// refused.
    ///
    /// So the refusal happens **before any plan is minted**: `EXDEV`, no
    /// durable effect, no intent, no fail-stop. It mirrors the owner-side
    /// post-discovery checks (`meta_ship::service`) so the two paths
    /// cannot drift.
    ///
    /// Unarmed — every mount that ships — this is `owns_volume`'s single
    /// relaxed load per participant feeding a never-taken branch.
    fn refuse_cross_owner_participants(
        &self,
        verb: crate::meta_ship::MetaVerb,
        participants: &[Ino],
    ) -> Result<()> {
        if !crate::meta_ship::owners::ownership_armed()
            || TEST_DISABLE_CROSS_OWNER_PRECHECK.load(std::sync::atomic::Ordering::Relaxed)
            // Executing AS THE OWNER for a shipping client: the authority
            // in force is the owner service's own per-volume vector, and
            // its post-discovery cross-owner checks have already run on
            // this very call. A dual-role node is both client and owner
            // (which is what the S8 service is designed for), so reading
            // the CLIENT-side map here would refuse verbs the owner half
            // is authoritative for — pinned by `tests/meta_ship_tests.rs`,
            // which is exactly that shape in one process.
            || crate::meta_ship::executing_for_ship_client()
        {
            return Ok(());
        }
        for ino in participants {
            let (v_idx, _) = self.route_ino(*ino);
            if let Some(owner) = crate::meta_ship::owners::owner_of_volume(v_idx) {
                return Err(crate::meta_ship::cross_owner_refusal(
                    verb,
                    *ino,
                    &format!(
                        "it lives on metadata volume {v_idx}, which peer '{}' appends to, \
                         while this node executes the operation. The participant was \
                         DISCOVERED under this op's guards, so the router could not see it \
                         (§5.4a M1)",
                        owner.peer_id
                    ),
                ));
            }
        }
        Ok(())
    }

    /// §4.4 pt 4 escalation mirror: after any mutation error, latch the
    /// volume into `disabled_volumes` iff its backend has fail-stopped
    /// (repeated journal write failures) — the existing mechanism
    /// `check_volume_enabled` consults.
    fn mirror_volume_failure(&self, idx: usize) {
        if self.volumes[idx].is_failed() {
            self.disabled_volumes.insert(idx, true);
        }
    }

    // -----------------------------------------------------------------
    // Symmetric PR 6 — the cross-owner arms' helpers (design §5.6,
    // §5.6.4). Every one is a no-op or a plain delegate on an unarmed
    // mount: the shipped paths pay one `Option` test.
    // -----------------------------------------------------------------

    /// The cross-owner arms' 4a acquisition (design §5.6 line 1 — "plan
    /// under the op's 4a guards; foreign-home guards travel"):
    /// `crossvol_tx::acquire_guards_leased` — every key whose lock lives
    /// in this process's table in the ONE canonical `lock_many`, every
    /// foreign holder's keys as one travelling `XvGuards` parked at the
    /// holder for the op's duration, tables in ascending appender-id
    /// order (deadlock-free by the hierarchical argument). `scope` is the
    /// op's, shared by its per-volume calls. Inside a served verb this
    /// mount IS the holder and every key is its own; unarmed: `lock_many`
    /// verbatim.
    async fn lock_many_leased(
        &self,
        v_idx: usize,
        scope: &mut Option<u64>,
        inos: &[(Ino, dlm::LockMode)],
        dents: &[(Ino, &str, dlm::LockMode)],
    ) -> Result<Vec<dlm::DlmGuard>> {
        crossvol_tx::acquire_guards_leased(self, v_idx, scope, inos, dents, false).await
    }

    /// [`Self::lock_many_leased`] for a DISCOVERY phase whose read is
    /// revalidated under the op's full set: foreign tables are not
    /// acquired (Issue 13 — one round trip per holder per op).
    async fn lock_many_leased_discovery(
        &self,
        v_idx: usize,
        inos: &[(Ino, dlm::LockMode)],
        dents: &[(Ino, &str, dlm::LockMode)],
    ) -> Result<Vec<dlm::DlmGuard>> {
        let mut scope = None;
        crossvol_tx::acquire_guards_leased(self, v_idx, &mut scope, inos, dents, true).await
    }

    /// Whether any of `inos` lives in a slot another appender leases —
    /// the arms' "this op is a cross-owner transaction" predicate.
    fn spans_foreign_slot(&self, inos: &[Ino]) -> bool {
        crossvol_tx::spans_foreign_slot(self, inos)
    }

    /// **The served side of a shipped step** (§5.6 — `xv_apply_step` is
    /// the ONE applier; this is the wire's door to it): resolve the step
    /// to its volume, refuse unless THIS mount leases its slot (a stale
    /// holder view at the initiator — it re-resolves through tree 0),
    /// take the step's 4a guards here, apply. The reply follows the
    /// commit's durability lane by construction.
    pub async fn xv_serve_step(
        &self,
        tx_id: u64,
        step_idx: u32,
        step: &crossvol_tx::XvStep,
        scope: crossvol_tx::GuardScope<'_>,
    ) -> Result<crossvol_tx::XvStepOutcome> {
        let (v_idx, local) = crossvol_tx::localise_step(self, step);
        self.check_volume_enabled(v_idx)?;
        let vol = &self.volumes[v_idx];
        if let Some(plane) = vol.slot_leases() {
            let slot = kv::record::forest_slot_of_ino(local.local_home());
            if !plane.gate.is_leased(slot) {
                let holder = match plane.table.resolve(slot) {
                    crate::slot_lease_core::Resolved::Holder { holder, g } => {
                        format!("appender {holder} at g {g}")
                    }
                    crate::slot_lease_core::Resolved::Unleased { .. } => "nobody".to_string(),
                };
                return Err(crate::error::SqueezefsError::refused(
                    libc::EAGAIN,
                    format!(
                        "cross-owner step {} of {tx_id:016x} homes on forest slot {slot}, which \
                         this mount does not lease ({holder} does) — the initiator's holder \
                         view is stale; it re-resolves through tree 0",
                        step.name()
                    ),
                ));
            }
        }
        // The wire's `child` (Issue 8a — PR 3's bounded-execution law): an
        // insert may name only an ino that already has a record on its
        // volume or lives in a slot tree 0 says SOME appender leases (the
        // creator's rotor slot — a fresh mint's record may not yet be in
        // this holder's projection of that slot; an unleased slot's ino is
        // one nobody could have minted). Anything else is a dangling
        // dentry a buggy peer would plant — refused, counted.
        if let crossvol_tx::XvStep::InsertDentry { child, .. } = step {
            self.screen_insert_child(*child, tx_id).await?;
        }
        // Under a scope whose guards COVER the step's keys they are held
        // already (the initiator's, in this table or parked here) and the
        // apply takes nothing — a served step never parks on a 4a guard;
        // otherwise it takes the step's keys for the apply's duration.
        let needed = crossvol_tx::step_stripes(vol.dlm(), v_idx, &local);
        let guards: std::sync::Arc<[dlm::DlmGuard]> =
            match crossvol_tx::serve_guards_for(scope, &needed) {
                crossvol_tx::ServeGuards::Covered => std::sync::Arc::from(Vec::new()),
                crossvol_tx::ServeGuards::Take => {
                    let (inos, dents) = crossvol_tx::step_guard_keys(&local);
                    let d: Vec<(Ino, &str, dlm::LockMode)> = dents
                        .iter()
                        .map(|(p, n)| (*p, n.as_str(), dlm::LockMode::Exclusive))
                        .collect();
                    std::sync::Arc::from(vol.dlm().lock_many(&inos, &d).await)
                }
            };
        // PR 7b (design §5.6.5): a served insert is one foreign ship into
        // its parent — the flip trigger's census reads the CREATOR off the
        // child's slot (the child was minted in the creator's rotor). A
        // parent whose record reads `nlink 0` (a stripe an `rmdir`'s
        // intent marked dying, a directory removed while open) refuses
        // the insert — R26's closer, read UNDER the step's guards (the
        // `Take` arm's included; review round 1, Issue 6) and answered as
        // a WITNESS refusal: a live plan stops and compensates (the
        // initiator maps it to `ENOENT` off the parent's record), a
        // roll-forward retires the intent instead of erroring at every
        // cadence pass (Issue 13).
        if let crossvol_tx::XvStep::InsertDentry {
            parent,
            child,
            name,
            ..
        } = step
        {
            if self.refuse_dying_parent(*parent).await.is_err() {
                let out = crossvol_tx::XvStepOutcome {
                    status: crossvol_tx::XvStepStatus::ForeignSkipped,
                    inode: None,
                };
                out.count();
                crossvol_tx::note_step_served();
                log::debug!(
                    "served cross-owner step {step_idx} (insert_dentry) of {tx_id:016x}: the \
                     parent {parent} is being removed — witness refusal"
                );
                return Ok(out);
            }
            if let Some(creator) = self.holder_of(*child) {
                if Some(creator) != self.holder_of(*parent) {
                    self.note_served_insert(*parent, name, creator);
                }
            }
        }
        let served_at = std::time::Instant::now();
        let out = vol.xv_apply_step(&local, None, guards, true).await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        let out = out?;
        out.count();
        crossvol_tx::note_step_served();
        log::debug!(
            "served cross-owner step {step_idx} ({}) of {tx_id:016x}: {:?}",
            step.name(),
            out.status
        );
        // PR 4's holder-side dominance evaluation AT THE SERVED SHIP
        // (§5.1.4 — `note_slot_ship`): before PR 13 nothing in the served
        // path called it, so `ops_q` never accumulated, `slot_offers` and
        // the idle arm read 0 on every fleet, and a dominating requester
        // never earned an idle holder's tree (gate 3c's IDLE row). The
        // requester is the shipping mount's appender: its member id's
        // identity where this plane learnt it, else — for an insert — the
        // creator through the child's slot (the child is minted in the
        // creator's rotor; the screen above refreshed the projection). A
        // ship into a STRIPED directory or one of its stripes is the
        // striping mechanism's (aggregate by construction) and feeds no
        // window (`is_striping_domain`).
        let striping = match step {
            crossvol_tx::XvStep::InsertDentry { parent, .. }
            | crossvol_tx::XvStep::RemoveDentry { parent, .. } => self.is_striping_domain(*parent),
            _ => false,
        };
        if vol.slot_leases().is_some() && !striping {
            let slot = kv::record::forest_slot_of_ino(local.local_home());
            if let Some(requester) = self.served_step_requester(vol, scope.client, step) {
                let ship_ns = u64::try_from(served_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
                let _ = vol.note_slot_ship(slot, requester, ship_ns).await;
            }
        }
        Ok(out)
    }

    /// The appender id of a served step's initiator — see the note at its
    /// one call site. `None` when neither the member identity nor the
    /// child's slot names one (the ship is served, never counted).
    fn served_step_requester(
        &self,
        vol: &kv::backend::KvMetaBackend,
        client: &str,
        step: &crossvol_tx::XvStep,
    ) -> Option<u32> {
        let plane = vol.slot_leases()?;
        let own = vol.own_appender_id();
        if let Some((node_token, mount_slot)) = crate::cowriter::parse_node_member_id(client) {
            if let Some(id) = plane.appender_of_identity(node_token, mount_slot) {
                return (id != own).then_some(id);
            }
        }
        if let crossvol_tx::XvStep::InsertDentry { child, .. } = step {
            let creator = self.holder_of(*child)?;
            return (creator != own).then_some(creator);
        }
        None
    }

    /// The served insert's `child` screen (Issue 8a): a record on its
    /// volume, or a slot some appender leases per tree 0; else refused
    /// (`EINVAL`, `xv_cross_owner_steps_rejected`). On a JOINED holder the
    /// lease table is a projection — refreshed once when it says
    /// `Unleased` (PR 13: a later joiner's slots were unknown to every
    /// earlier joiner until some event advanced its projection, and the
    /// fleet's first joiner→joiner create was refused here).
    async fn screen_insert_child(&self, child: Ino, tx_id: u64) -> Result<()> {
        let (v_idx, local) = self.route_ino(child);
        self.check_volume_enabled(v_idx)?;
        let vol = &self.volumes[v_idx];
        if vol.slot_leases().is_some() {
            let slot = kv::record::forest_slot_of_ino(local);
            if matches!(
                vol.resolve_slot_holder_fresh(slot).await,
                Some(crate::slot_lease_core::Resolved::Holder { .. })
            ) {
                return Ok(());
            }
        }
        if vol.read_inode_value_routed(local).await?.is_some() {
            return Ok(());
        }
        crossvol_tx::note_step_rejected();
        Err(crate::error::SqueezefsError::refused(
            libc::EINVAL,
            format!(
                "cross-owner step of {tx_id:016x}: insert names child ino {child}, which has no \
                 inode record and lives in a slot no appender leases — a dentry nobody could \
                 have minted a target for is refused (xv_cross_owner_steps_rejected)"
            ),
        ))
    }

    /// **The served half of a travelling guard** (`MetaCall::XvGuards`):
    /// park the initiator's 4a guards on `inodes` / `dentries` (global
    /// keys, one volume) under `(client, scope)` — ONE canonical
    /// `lock_many` in this table, held until `XvRelease` or the
    /// initiator's lease expiry. Refuses (EAGAIN) a key whose slot this
    /// mount does not lease — the initiator's holder view is stale. A
    /// stripe the scope already parked is skipped (the same scope's
    /// second call, or a resend past the dedup window, must never wait
    /// behind itself).
    pub async fn xv_serve_guards(
        &self,
        client: &str,
        scope: u64,
        inodes: &[(u64, bool)],
        dentries: &[(u64, String, bool)],
    ) -> Result<()> {
        let mode = |exclusive: bool| {
            if exclusive {
                dlm::LockMode::Exclusive
            } else {
                dlm::LockMode::Shared
            }
        };
        let mut per_vol: std::collections::BTreeMap<
            usize,
            (Vec<(Ino, dlm::LockMode)>, Vec<(Ino, String, dlm::LockMode)>),
        > = std::collections::BTreeMap::new();
        for (ino, exclusive) in inodes {
            let (v, l) = self.route_ino(*ino);
            per_vol.entry(v).or_default().0.push((l, mode(*exclusive)));
        }
        for (parent, name, exclusive) in dentries {
            let (v, l) = self.route_ino(*parent);
            per_vol
                .entry(v)
                .or_default()
                .1
                .push((l, name.clone(), mode(*exclusive)));
        }
        for (v_idx, (inos, dents)) in per_vol {
            self.check_volume_enabled(v_idx)?;
            let vol = &self.volumes[v_idx];
            if let Some(plane) = vol.slot_leases() {
                for local in inos
                    .iter()
                    .map(|(l, _)| *l)
                    .chain(dents.iter().map(|(l, _, _)| *l))
                {
                    let slot = kv::record::forest_slot_of_ino(local);
                    if !plane.gate.is_leased(slot) {
                        return Err(crate::error::SqueezefsError::refused(
                            libc::EAGAIN,
                            format!(
                                "cross-owner guards for scope {scope:#x} name forest slot \
                                 {slot}, which this mount does not lease — the initiator's \
                                 holder view is stale; it re-resolves through tree 0"
                            ),
                        ));
                    }
                }
            }
            let dlm = vol.dlm();
            let parked = crossvol_tx::parked_stripes(client, scope);
            let inos: Vec<(Ino, dlm::LockMode)> = inos
                .into_iter()
                .filter(|(l, _)| !parked.contains(&(v_idx, true, dlm.inode_stripe(*l))))
                .collect();
            let dents: Vec<(Ino, String, dlm::LockMode)> = dents
                .into_iter()
                .filter(|(l, n, _)| !parked.contains(&(v_idx, false, dlm.dentry_stripe(*l, n))))
                .collect();
            if inos.is_empty() && dents.is_empty() {
                continue;
            }
            let stripes: Vec<(usize, bool, usize)> = inos
                .iter()
                .map(|(l, _)| (v_idx, true, dlm.inode_stripe(*l)))
                .chain(
                    dents
                        .iter()
                        .map(|(l, n, _)| (v_idx, false, dlm.dentry_stripe(*l, n))),
                )
                .collect();
            let d: Vec<(Ino, &str, dlm::LockMode)> =
                dents.iter().map(|(l, n, m)| (*l, n.as_str(), *m)).collect();
            log::debug!(
                "cross-owner guards: serving scope {scope:#x} for '{client}' on volume {v_idx} — \
                 taking {} inode + {} dentry guard(s) (stripes {stripes:?})",
                inos.len(),
                d.len()
            );
            let t0 = log::log_enabled!(log::Level::Debug).then(std::time::Instant::now);
            let guards = dlm.lock_many(&inos, &d).await;
            if let Some(t0) = t0 {
                log::debug!(
                    "cross-owner guards: scope {scope:#x} for '{client}' on volume {v_idx} \
                     parked in {:?}",
                    t0.elapsed()
                );
            }
            crossvol_tx::park_guards(client, scope, guards, stripes);
        }
        Ok(())
    }

    /// Feed the parent memo after a rename: a moved DIRECTORY's one name
    /// is `(new_parent, new_name)`; under `RENAME_EXCHANGE` the swapped
    /// directory's is `(old_parent, old_name)`.
    fn note_renamed_dir_parents(
        &self,
        moved: Option<(Ino, u32)>,
        dest: Option<(Ino, u32)>,
        flags: u32,
        old: (Ino, &str),
        new: (Ino, &str),
    ) {
        if let Some((child, ft)) = moved {
            if ft == libc::S_IFDIR {
                self.note_dir_parent(child, new.0, new.1);
            }
        }
        if flags & libc::RENAME_EXCHANGE != 0 {
            if let Some((child, ft)) = dest {
                if ft == libc::S_IFDIR {
                    self.note_dir_parent(child, old.0, old.1);
                }
            }
        } else if let Some((victim, _)) = dest {
            self.dir_parents.invalidate(&victim);
        }
    }

    /// Refuse (EAGAIN) a served read or step naming an ino whose slot
    /// this mount does not lease — the initiator's holder view is stale
    /// and it re-resolves through tree 0. A no-op on an unarmed volume.
    pub(crate) fn refuse_unless_slot_leased_here(&self, ino: Ino, what: &str) -> Result<()> {
        let (v_idx, local) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let Some(plane) = self.volumes[v_idx].slot_leases() else {
            return Ok(());
        };
        let slot = kv::record::forest_slot_of_ino(local);
        if plane.gate.is_leased(slot) {
            return Ok(());
        }
        let holder = match plane.table.resolve(slot) {
            crate::slot_lease_core::Resolved::Holder { holder, g } => {
                format!("appender {holder} at g {g}")
            }
            crate::slot_lease_core::Resolved::Unleased { .. } => "nobody".to_string(),
        };
        Err(crate::error::SqueezefsError::refused(
            libc::EAGAIN,
            format!(
                "{what} names ino {ino} on forest slot {slot}, which this mount does not lease \
                 ({holder} does) — the initiator's holder view is stale; it re-resolves \
                 through tree 0"
            ),
        ))
    }

    /// The GLOBAL parent of directory `dir` (the one dentry naming it),
    /// `None` for the root or an unreferenced directory — the memo's
    /// hint, else the reverse dentry scan (`find_parent_of_child`'s
    /// class, `meta_parent_scans`; v3 keeps no parent pointer). Test
    /// support for the cross-owner contracts' chain walks (the product
    /// walk is `refuse_rename_into_own_subtree`, which confirms each hop).
    pub async fn parent_of_directory(&self, dir: Ino) -> Result<Option<Ino>> {
        if let Some((p, _)) = self.dir_parents.get(&dir) {
            return Ok(Some(p));
        }
        Ok(self.scan_parent_link_of(dir).await?.map(|(p, _)| p))
    }

    /// The reverse dentry scan for `dir`'s one name — the memo miss path.
    async fn scan_parent_link_of(&self, dir: Ino) -> Result<Option<(Ino, String)>> {
        if dir == kv::builder::ROOT_INO {
            return Ok(None);
        }
        for (v_idx, vol) in self.volumes.iter().enumerate() {
            if let Some((local_parent, name)) = vol.find_parent_link_of_child(dir).await? {
                return Ok(self
                    .try_make_global_ino(local_parent, v_idx)
                    .map(|p| (p, name)));
            }
        }
        Ok(None)
    }

    /// Note a directory's one name in the parent memo (a mint, a
    /// directory rename, a confirmed ancestor hop). A hint only.
    pub(crate) fn note_dir_parent(&self, dir: Ino, parent: Ino, name: &str) {
        self.dir_parents
            .insert(dir, (parent, std::sync::Arc::from(name)));
    }

    /// The EXACT `(parent, name)` read WITHOUT a 4a guard: the local arm
    /// of the directory-rename ancestor check and the served
    /// `LookupExact` (review round 1, Issue 2). The walk runs while the
    /// initiator HOLDS its own exclusive `D{}` guards, so a shared `D{}`
    /// guard here would park the initiator behind itself on a stripe
    /// collision — with the set-wide lease held, wedging every directory
    /// rename in the set. The lease is what makes the read consistent:
    /// no other directory rename can move the chain under it, and a
    /// `mkdir`/`rmdir` never re-parents an existing directory.
    pub async fn lookup_dentry_exact_unguarded(
        &self,
        parent: Ino,
        name: &str,
    ) -> Result<Option<(Ino, u32)>> {
        // PR 7b: a striped parent's name is read where it lives (its
        // stripe, or the directory's own tree while migrating).
        if let Some(route) = self.stripe_route(parent, name).await? {
            return Ok(self
                .stripe_locate(parent, name, &route)
                .await?
                .map(|(_, child, ft)| (child, ft)));
        }
        let (v_idx, local_parent) = self.route_ino(parent);
        self.check_volume_enabled(v_idx)?;
        self.find_dentry_routed(v_idx, local_parent, name).await
    }

    /// The directory-rename ANCESTOR CHECK on exact data (§5.6.4): under
    /// the set-wide lock, walk `new_parent`'s chain to the root and refuse
    /// `EINVAL` if `moved` is on it. Each hop's link `(p, name) → d` comes
    /// from the parent memo (O(1)) or, on a miss, the reverse dentry scan
    /// (`dir_rename_parent_scans`), and is CONFIRMED at `p`'s slot holder
    /// through an exact guard-free lookup (its RAM-authoritative tree —
    /// one RPC per foreign link, no 4a guard: Issue 2). A denied or
    /// absent link is a projection not yet showing a previous lock
    /// holder's rename: a stale memo hint falls to the scan at once, a
    /// scanned link is re-read after one publication ceiling, bounded —
    /// `EAGAIN` past the bound, never a guess.
    async fn refuse_rename_into_own_subtree(&self, moved: Ino, new_parent: Ino) -> Result<()> {
        let einval =
            || crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(libc::EINVAL));
        if moved == new_parent {
            return Err(einval());
        }
        let ceiling = kv::checkpoint::checkpoint_landing_ceiling_derived();
        let mut cur = new_parent;
        let mut hops = 0usize;
        while cur != kv::builder::ROOT_INO {
            if cur == moved {
                return Err(einval());
            }
            let mut confirmed: Option<Ino> = None;
            let mut memo_hint = self.dir_parents.get(&cur);
            let mut scans = 0u32;
            // Three SCANNED attempts a ceiling apart (a stale memo hint
            // costs no wait — it falls straight to the scan).
            while scans < 3 {
                let link = match memo_hint.take() {
                    Some((p, name)) => Some((p, name.to_string())),
                    None => {
                        scans += 1;
                        crossvol_tx::note_dir_rename_parent_scan();
                        self.scan_parent_link_of(cur).await?
                    }
                };
                if let Some((p, name)) = link {
                    let exact = crossvol_tx::lookup_exact(self, p, &name).await?;
                    if exact.map(|(c, _)| c) == Some(cur) {
                        self.note_dir_parent(cur, p, &name);
                        confirmed = Some(p);
                        break;
                    }
                    self.dir_parents.invalidate(&cur);
                    if scans == 0 {
                        continue;
                    }
                }
                if scans < 3 {
                    squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(ceiling)).await;
                }
            }
            let Some(p) = confirmed else {
                return Err(crate::error::SqueezefsError::refused(
                    libc::EAGAIN,
                    format!(
                        "directory rename: ancestor {cur} of the target has no name this \
                         mount can confirm at its holder after {} ms — retry",
                        3 * ceiling
                    ),
                ));
            };
            cur = p;
            hops += 1;
            if hops > 4096 {
                // Deeper than any legal tree: a cycle already exists —
                // refuse rather than walk it for ever.
                return Err(einval());
            }
        }
        Ok(())
    }

    /// Rename fragment (PR M6 D4.b): stamp the moved/exchanged inode's
    /// ctime when it lives on a DIFFERENT volume than the dentry surgery
    /// (the same-volume path stages it inside the rename tx).
    async fn touch_ctime_routed(
        &self,
        idx: usize,
        local_ino: Ino,
        guards: std::sync::Arc<[dlm::DlmGuard]>,
    ) -> Result<()> {
        self.check_volume_enabled(idx)?;
        let out = self.volumes[idx]
            .routed_touch_ctime(local_ino, guards)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(idx);
        }
        out
    }

    /// Rename fragment: destination-inode replacement accounting —
    /// ENOTEMPTY probe for directories, nlink dec + ctime (best-effort on
    /// a missing inode).
    async fn dest_replace_routed(
        &self,
        idx: usize,
        local_dest: Ino,
        guards: std::sync::Arc<[dlm::DlmGuard]>,
    ) -> Result<()> {
        let out = self.volumes[idx]
            .routed_dest_replace(local_dest, guards)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(idx);
        }
        out
    }

    /// LOCK-FREE inode read (no DLM acquisition — safe under held routed
    /// I-guards, where a re-entrant stripe read can deadlock against a
    /// queued writer).
    async fn read_inode_routed(&self, idx: usize, local_ino: Ino) -> Result<Inode> {
        self.volumes[idx].getattr(local_ino).await
    }

    /// The per-volume metadata lock manager (tests force stripe collisions
    /// through its public stripe accessors).
    pub fn volume_dlm(&self, idx: usize) -> &dlm::DlmLockManager {
        self.volumes[idx].dlm()
    }

    pub fn check_volume_enabled(&self, idx: usize) -> Result<()> {
        if self.disabled_volumes.contains_key(&idx) {
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                format!("Metadata volume {} is disabled", idx),
            )));
        }
        Ok(())
    }

    pub async fn get_volume_health(&self, idx: usize) -> u32 {
        if self.disabled_volumes.contains_key(&idx) {
            return 0;
        }
        if idx >= self.volumes.len() {
            return 0;
        }
        // Health only used for dir placement; a short cache avoids scanning
        // allocator occupancy on every mkdir under multi-thread load.
        static HEALTH_CACHE: once_cell::sync::Lazy<scc::HashMap<usize, (u32, std::time::Instant)>> =
            once_cell::sync::Lazy::new(scc::HashMap::new);
        if let Some(v) = HEALTH_CACHE.read_sync(&idx, |_, v| *v) {
            if v.1.elapsed() < std::time::Duration::from_millis(500) {
                return v.0;
            }
        }
        // Estimated remaining-capacity FRACTION: free extents / total
        // (resolved OQ 5, design §4.9).
        let be = &self.volumes[idx];
        let total = be.superblock().total_extents();
        let free_factor = if total > 0 {
            be.free_extents() as f64 / total as f64
        } else {
            0.0
        };

        let score = (free_factor * 1000.0) as u32;
        let score = score.min(1000);
        let _ = HEALTH_CACHE.upsert_sync(idx, (score, std::time::Instant::now()));
        score
    }

    /// Health-banded round-robin mint placement for freshly created
    /// inodes — directories AND regular files (perf/meta-plane-writes,
    /// 2026-07-30; contract in `tests/meta_plane_distribution_tests.rs`).
    ///
    /// History: only directories striped; regular files were
    /// parent-sticky (`target = parent volume`). Because every
    /// data-plane meta commit (block publish / size flip / extent
    /// spill / destroy) routes by the FILE's ino, a workload whose
    /// files live under one directory — the field's 32 root-dir files
    /// — drove 100 % of journal traffic into one volume's journal +
    /// conveyor (~21-25 k meta device-writes/s ceiling) while its
    /// sibling idled at 0.00. Minting files across the healthy band
    /// spreads the per-volume journal/conveyor/checkpoint pipelines,
    /// which is what the multi-volume meta plane exists for.
    ///
    /// Economy note: a cross-volume regular create costs two whole-tx
    /// entries (mint on target + dentry/parent-times on parent) instead
    /// of one — but they land on DIFFERENT volumes, so per-volume
    /// entries/op stays ≈ 1 and the create ceiling is unchanged while
    /// the data plane gains the full set's journal bandwidth. Single-
    /// volume sets short-circuit to volume 0 (no health probe on the
    /// hot path — exactly the pre-VL5a create shape).
    async fn pick_mint_volume(&self, parent_v_idx: usize) -> usize {
        if self.volumes.len() <= 1 {
            return parent_v_idx;
        }
        // **The owned-candidate filter** (per-volume claim admission
        // §5.5.1): while a multi-owner plane is armed, propose only
        // volumes this node OWNS. Without it the health rotor proposes a
        // peer's volume on (K−1)/K of picks and `constrain_mint_volume`
        // redirects them to the PARENT's — so `mint_redirects` grows
        // structurally on every node (carrying no signal at all) and a
        // node owning two volumes gets no balance among its own.
        //
        // A PREFERENCE, never a gate (Issue 29): `disabled_volumes` is
        // populated at RUNTIME by the fail-stop lattice, so the filtered
        // set can be empty on a node that owned volumes at admission
        // time. The empty case falls through to the existing
        // `candidates.is_empty()` arm below — the parent's volume, which
        // M2 makes owned by construction — and `check_volume_enabled`
        // turns "that one is disabled too" into a clean typed error.
        // Never a panic, never a new refusal.
        let armed = crate::meta_ship::ownership_armed();
        let mut candidates = Vec::new();
        for (i, _) in self.volumes.iter().enumerate() {
            if self.disabled_volumes.contains_key(&i) {
                continue;
            }
            if armed && !crate::meta_ship::owners::owns_volume(i) {
                continue;
            }
            let health = self.get_volume_health(i).await;
            candidates.push((i, health));
        }
        if candidates.is_empty() {
            return parent_v_idx;
        }
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        let max_health = candidates[0].1;
        let top_candidates: Vec<_> = candidates
            .into_iter()
            .filter(|c| c.1 >= (max_health * 9) / 10)
            .collect();

        static META_COUNTER: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let idx =
            META_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % top_candidates.len();
        top_candidates[idx].0
    }

    /// Batched reclaim (design-wal-crash-consistency §4.5): group the inos
    /// by owning volume and destroy each volume's group as one
    /// `destroy_inodes` transaction. All-or-nothing per call — the first
    /// failing volume aborts and the caller bisects (volume grouping is
    /// deterministic, so halves re-route consistently and converge to
    /// singletons).
    pub async fn destroy_inodes(&self, inos: &[Ino]) -> Result<()> {
        // §5.5.2a cutover gate — the whole batch's slots, before any 4a.
        let _gate = self.slot_gate_enter(inos).await;
        let mut per_volume: std::collections::HashMap<usize, Vec<Ino>> =
            std::collections::HashMap::new();
        for &ino in inos {
            let (v_idx, local_ino) = self.route_ino(ino);
            per_volume.entry(v_idx).or_default().push(local_ino);
        }
        for (v_idx, locals) in per_volume {
            self.check_volume_enabled(v_idx)?;
            self.volumes[v_idx].destroy_inodes(&locals).await?;
        }
        Ok(())
    }

    /// The journal payload `ino`'s destroy stages on its home volume —
    /// the inode `Delete` plus one per xattr, in the admission's own
    /// framing ([`kv::backend::KvMetaBackend::destroy_entry_bytes`]). The
    /// reclaim planner's record term; the release term is
    /// [`Self::release_records_bytes`].
    pub async fn destroy_entry_bytes(&self, ino: Ino) -> Result<u64> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        self.volumes[v_idx].destroy_entry_bytes(local_ino).await
    }

    /// The journal payload `n` reference releases stage on `ino`'s home
    /// volume, framed as that volume STAGES them (a forest volume's keys
    /// carry a kind byte — [`kv::backend::KvMetaBackend::release_records_bytes`]).
    pub fn release_records_bytes(&self, ino: Ino, n: usize) -> u64 {
        let (v_idx, _) = self.route_ino(ino);
        self.volumes[v_idx].release_records_bytes(n)
    }

    /// RECLAIM-ATOMIC: [`Self::destroy_inodes`] with every ino's durable
    /// reference releases riding the SAME journal entry
    /// ([`kv::backend::KvMetaBackend::destroy_inodes_releasing`]). ONE
    /// entry, so `items` must share a home volume — the reclaim planner
    /// groups by [`Self::route_ino`] first; a set spanning volumes is
    /// refused before anything commits (a later volume's failure could
    /// otherwise lose an earlier volume's verdicts).
    pub async fn destroy_inodes_releasing(
        &self,
        items: &[(Ino, &[kv::block_refs::BlockRefOp])],
    ) -> Result<Vec<(Ino, kv::block_refs::DestroyVerdict)>> {
        let Some(&(first, _)) = items.first() else {
            return Ok(Vec::new());
        };
        let inos: Vec<Ino> = items.iter().map(|&(ino, _)| ino).collect();
        let _gate = self.slot_gate_enter(&inos).await;
        let (v_idx, _) = self.route_ino(first);
        let mut translated: Vec<std::borrow::Cow<'_, [kv::block_refs::BlockRefOp]>> =
            Vec::with_capacity(items.len());
        let mut local_inos: Vec<Ino> = Vec::with_capacity(items.len());
        for &(ino, ops) in items {
            let (v, local_ino) = self.route_ino(ino);
            if v != v_idx {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "destroy_inodes_releasing: ino {ino} homes on volume {v}, the set's first \
                     ino {first} on {v_idx} — one entry cannot span volumes (group by home \
                     volume first)"
                )));
            }
            local_inos.push(local_ino);
            translated.push(self.forest_ref_ops(v_idx, ops));
        }
        let locals: Vec<(Ino, &[kv::block_refs::BlockRefOp])> = local_inos
            .iter()
            .copied()
            .zip(translated.iter().map(|c| c.as_ref()))
            .collect();
        self.check_volume_enabled(v_idx)?;
        let out = self.volumes[v_idx].destroy_inodes_releasing(&locals).await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        Ok(inos.into_iter().zip(out?).collect())
    }

    /// **A forest volume's block references key their owner's LOCAL KEY
    /// ino** (symmetric metadata PR 7 — a PR-1 defect, pinned by
    /// `tests/sym_pack_tests.rs`): the forest codec routes a reference by
    /// the top 24 bits of the owner at offset 17 — the `(s+1) << 40 |
    /// local` form whose top bits ARE the slot — while every product site
    /// keys the owner's GLOBAL ino (the rung-19 law, `refs_owner`), whose
    /// top bits are zero for any realistic ino. Through the routed layer
    /// every reference therefore landed in the NATIVE slot tree (the
    /// manager's) whatever slot its owner lived in: complete as a census,
    /// wrong for the plane (a publish from another appender named the
    /// manager's slot — the cross-region refusal) and fatal to the pack
    /// law's one-slot probe. This layer owns `route_ino`, so it rewrites
    /// each op's owner to the key form the volume routes by; an owner that
    /// homes elsewhere (a shipped frame's foreign slice) is left as the
    /// kv layer keys it. A FLAT volume takes the ops verbatim — its ledger
    /// stays byte-identical.
    fn forest_ref_ops<'a>(
        &self,
        v_idx: usize,
        ops: &'a [kv::block_refs::BlockRefOp],
    ) -> std::borrow::Cow<'a, [kv::block_refs::BlockRefOp]> {
        if ops.is_empty() || !self.volumes[v_idx].symmetric_forest() {
            return std::borrow::Cow::Borrowed(ops);
        }
        std::borrow::Cow::Owned(
            ops.iter()
                .map(|op| {
                    let (v, local) = self.route_ino(op.reference.owner_ino);
                    if v == v_idx {
                        kv::block_refs::BlockRefOp {
                            reference: kv::shared_refs::with_owner(&op.reference, local),
                            ..*op
                        }
                    } else {
                        *op
                    }
                })
                .collect(),
        )
    }

    /// The `refs_owner` a volume's recompute paths key their references
    /// on: the local key ino on a forest ([`Self::forest_ref_ops`]), the
    /// global ino everywhere else (the rung-19 law verbatim).
    fn forest_refs_owner(&self, v_idx: usize, ino: Ino, local_ino: Ino) -> Ino {
        if self.volumes[v_idx].symmetric_forest() {
            local_ino
        } else {
            ino
        }
    }

    /// RECLAIM-ATOMIC residual B: routed
    /// [`kv::backend::KvMetaBackend::destroy_inode_chunked`] — one ino
    /// whose releases + xattrs + record exceed the whole-entry cap,
    /// destroyed across entries on its home volume.
    pub async fn destroy_inode_chunked(
        &self,
        ino: Ino,
        refs: &[kv::block_refs::BlockRefOp],
    ) -> Result<kv::block_refs::ChunkedDestroy> {
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let refs = self.forest_ref_ops(v_idx, refs);
        let out = self.volumes[v_idx]
            .destroy_inode_chunked(local_ino, &refs)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    /// [`Self::destroy_inodes`] for records that no dentry names — fsck
    /// class C9's repair verb (see
    /// [`kv::backend::KvMetaBackend::destroy_unreferenced_inodes`] for why
    /// the live-`nlink` skip is deliberately not taken and why the destroy
    /// stays one transaction).
    pub async fn destroy_unreferenced_inodes(&self, inos: &[Ino]) -> Result<()> {
        let _gate = self.slot_gate_enter(inos).await;
        let mut per_volume: std::collections::HashMap<usize, Vec<Ino>> =
            std::collections::HashMap::new();
        for &ino in inos {
            let (v_idx, local_ino) = self.route_ino(ino);
            per_volume.entry(v_idx).or_default().push(local_ino);
        }
        for (v_idx, locals) in per_volume {
            self.check_volume_enabled(v_idx)?;
            self.volumes[v_idx]
                .destroy_unreferenced_inodes(&locals)
                .await?;
        }
        Ok(())
    }

    /// Global ino → `(volumes index, EFFECTIVE local ino)`:
    /// [`route_ino_width`] over the FROZEN W (never `volumes.len()` —
    /// PR VL5a, KD-7), then the slot→volume table, then the keyspace
    /// mapping (PR VL5b): a slot hosted as a GUEST serves from its
    /// ino-namespace partition (`guest_local_ino`); the host's legacy
    /// slot serves un-namespaced. `W ≤ 1` keeps the pre-VL5a identity
    /// short-circuit verbatim.
    pub fn route_ino(&self, ino: Ino) -> (usize, Ino) {
        if self.routing_width <= 1 {
            return (0, ino);
        }
        let (slot, local_ino) = route_ino_width(ino, self.routing_width);
        let t = self.route.load();
        let v = t.slot_to_volume[slot as usize];
        let eff = if self.legacy_slot[v] == Some(slot as u16) {
            local_ino
        } else {
            guest_local_ino(slot as u16, local_ino)
        };
        (v, eff)
    }

    /// The slot a global ino routes through (gate/tee attribution).
    pub fn slot_of_ino(&self, ino: Ino) -> u64 {
        if self.routing_width <= 1 {
            return 0;
        }
        route_ino_width(ino, self.routing_width).0
    }

    /// The volume currently hosting `slot` — one arc-swap load + an
    /// index (the rung-14 placement policy's live-home lookup). `None`
    /// for a slot outside the frozen width.
    pub fn slot_volume(&self, slot: u16) -> Option<usize> {
        if self.routing_width <= 1 {
            return (slot == 0).then_some(0);
        }
        let t = self.route.load();
        t.slot_to_volume.get(usize::from(slot)).copied()
    }

    /// `(EFFECTIVE local ino, volumes index)` → global ino over the
    /// frozen W: guest-namespaced locals carry their slot in the high
    /// bits; un-namespaced locals belong to the volume's legacy slot.
    pub fn make_global_ino(&self, local_ino: Ino, volume_idx: usize) -> Ino {
        if self.routing_width <= 1 {
            return local_ino;
        }
        match split_guest_local(local_ino) {
            Some((slot, raw)) => make_global_ino_width(raw, u64::from(slot), self.routing_width),
            None => {
                // Structural invariant: raw locals only ever exist in a
                // volume's legacy keyspace (guest keyspaces are minted
                // namespaced; local ino 1 control records never flow
                // through global encoding).
                let slot = self.legacy_slot[volume_idx]
                    .expect("raw local ino on a volume with no legacy keyspace (routing bug)");
                make_global_ino_width(local_ino, u64::from(slot), self.routing_width)
            }
        }
    }

    /// [`Self::make_global_ino`] for TREE WALKS (fsck / mover planners /
    /// defrag census): a raw local on a volume with NO legacy keyspace
    /// (a guest-only member added by `volume add-meta`) is a
    /// format-bootstrap CONTROL record, not a user inode — it has no
    /// global encoding and the walk must skip it, never panic (VL9
    /// soak-found: `fsck --offline` after add-meta panicked on the new
    /// member's local root record). The op-path invariant in
    /// [`Self::make_global_ino`] stays a panic — routed ops can only
    /// reach raw locals through the legacy keyspace.
    pub fn try_make_global_ino(&self, local_ino: Ino, volume_idx: usize) -> Option<Ino> {
        if self.routing_width <= 1 {
            return Some(local_ino);
        }
        // Raw local 1 is the ROOT PIN only in slot 0's keyspace; every
        // other keyspace's local 1 (native bootstrap or hosted-guest
        // `guest_local_ino(slot, 1)`) is a per-volume/per-slot CONTROL
        // record with no global encoding (its `(local - 2)` would
        // underflow). Locals < 1 never encode.
        let (slot, raw) = match split_guest_local(local_ino) {
            Some((slot, raw)) => (u64::from(slot), raw),
            None => (u64::from(self.legacy_slot[volume_idx]?), local_ino),
        };
        if raw == 1 {
            return (slot == 0).then_some(1);
        }
        if raw < 2 {
            return None;
        }
        Some(make_global_ino_width(raw, slot, self.routing_width))
    }

    /// Pick the slot the NEXT fresh ino on `volume_idx` mints into
    /// (design-dynamic-meta-routing §5.4): the per-volume rotor over the
    /// mint set — `min(MINT_SPREAD, hosted)` slots, so a volume's load
    /// is divisible into that many movable slices from birth. Callers on
    /// the gated op paths pick FIRST, gate the picked slot, then mint
    /// into it ([`Self::allocate_local_ino_in_slot`]).
    pub fn pick_mint_slot(&self, volume_idx: usize) -> u64 {
        if self.routing_width <= 1 {
            return 0;
        }
        // The armed symmetric plane (PR 4, KD-SYM-11): the mint lands in a
        // LEASED rotor slot — no parent here, so the rotor slot with the
        // most headroom (the overflow arm needs the async face).
        if let Some(slot) = self.lease_mint_slot(volume_idx, None) {
            return slot;
        }
        let t = self.route.load();
        // Rung 14 (client-owned-slot placement, KD-MW-6): a mint executing
        // FOR a shipping client (the owner-side SHIP_CLIENT scope, armed
        // plane, lever on) lands in that client's DEDICATED slot instead
        // of the shared rotor — outside the mint set, stable per client —
        // so the client's minted population is migratable as a unit. One
        // relaxed load + an absent task-local on every other mount; the
        // volume-level mint CONSTRAINT (`constrain_mint_volume`) already
        // ran above this pick, never below it.
        if let Some(slot) = crate::meta_ship::placement::client_mint_slot(
            volume_idx,
            &t.slot_to_volume,
            &t.mint_slots[volume_idx],
        ) {
            return slot;
        }
        let mints = &t.mint_slots[volume_idx];
        debug_assert!(!mints.is_empty(), "every volume hosts ≥ 1 slot");
        let k = self.mint_rr[volume_idx].fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % mints.len();
        u64::from(mints[k])
    }

    /// The armed plane's mint slot on `volume_idx` for a child of
    /// `parent` (design-symmetric-metadata §5.1.2): the parent's slot iff
    /// this mount leases it, it is not native and its tree is below
    /// `A_max(t)`; else the rotor slot with the most headroom. `None` on
    /// an unarmed volume. The synchronous face answers the SMALLEST rotor
    /// tree for the overflow arm; [`Self::pick_mint_slot_for`] asks the
    /// manager for one more slot instead.
    fn lease_mint_slot(&self, volume_idx: usize, parent: Option<Ino>) -> Option<u64> {
        let vol = self.volumes.get(volume_idx)?;
        if !vol.slot_lease_armed() {
            return None;
        }
        let choice = vol.lease_mint_choice(self.lease_parent_slot(volume_idx, parent))?;
        self.lease_choice_slot(volume_idx, choice)
    }

    /// The parent's forest slot for the mint policy — `None` when the
    /// parent is on another volume (no affinity across volumes) or its
    /// slot is another appender's (§5.6: a child of a foreign directory
    /// is minted in the creator's rotor — affinity never follows a
    /// foreign parent).
    fn lease_parent_slot(
        &self,
        volume_idx: usize,
        parent: Option<Ino>,
    ) -> Option<kv::record::ForestSlot> {
        parent.and_then(|p| {
            let (pv, local) = self.route_ino(p);
            (pv == volume_idx
                && matches!(
                    crossvol_tx::step_home(self, volume_idx, local),
                    crossvol_tx::StepHome::Local
                ))
            .then(|| kv::record::forest_slot_of_ino(local))
        })
    }

    /// A decided mint choice's ROUTING slot; the overflow arm's
    /// synchronous stand-in is the smallest rotor tree (the past-`2 × M`
    /// law).
    fn lease_choice_slot(
        &self,
        volume_idx: usize,
        choice: crate::slot_lease_core::MintChoice,
    ) -> Option<u64> {
        let vol = self.volumes.get(volume_idx)?;
        let forest = match choice {
            crate::slot_lease_core::MintChoice::Affinity(s)
            | crate::slot_lease_core::MintChoice::Rotor(s)
            | crate::slot_lease_core::MintChoice::Smallest(s) => s,
            crate::slot_lease_core::MintChoice::Overflow => {
                let plane = vol.slot_leases()?;
                let rotor = plane.rotor.load();
                *rotor.iter().min_by_key(|s| (plane.extents.get(**s), **s))?
            }
        };
        vol.routing_slot_of_forest(forest).ok().map(u64::from)
    }

    /// **Gather mode's mint arm** (design-symmetric-metadata §5.7.5,
    /// KD-SYM-17; symmetric PR 7): on an ARMED volume, a child of a
    /// directory that carries [`crate::GATHER_XATTR`] mints into the
    /// DIRECTORY's slot — from that slot's cursor, whatever the affinity
    /// ceiling says — so its inode lives beside the directory's entries
    /// and a `stat` of it needs no per-child token. `None` (the ordinary
    /// policy decides) when the parent is on another volume, in the
    /// native slot (control records and `/`'s dentries only — §5.1.2's
    /// exclusion holds here too), or carries no opt-in. NEVER automatic:
    /// the opt-in is the operator's `setfattr`, read by one node-cache
    /// probe of the parent's xattrs per create on an armed volume and
    /// nowhere else. The holder is this mount when it leases the slot;
    /// an unleased slot is acquired first-touch by the commit door, and a
    /// slot another appender leases refuses there (`SlotBusy` — the
    /// "ship to the holder" class PR 6/12 own), exactly as the parent's
    /// own dentry record does for every create into that directory.
    async fn gather_mint_slot(&self, volume_idx: usize, parent: Ino) -> Option<u64> {
        let (pv, local_parent) = self.route_ino(parent);
        if pv != volume_idx {
            return None;
        }
        let parent_slot = kv::record::forest_slot_of_ino(local_parent);
        if parent_slot == kv::record::NATIVE_FOREST_SLOT {
            return None;
        }
        let vol = self.volumes.get(volume_idx)?;
        let opted_in = vol
            .getxattr(local_parent, crate::GATHER_XATTR)
            .await
            .ok()
            .flatten()
            .is_some_and(|v| crate::gather_xattr_opts_in(&v));
        if !opted_in {
            return None;
        }
        let routing = vol.routing_slot_of_forest(parent_slot).ok()?;
        if let Some(plane) = vol.slot_leases() {
            plane
                .dir_gather_mints
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Some(u64::from(routing))
    }

    /// [`Self::pick_mint_slot`] for a child of `parent` — the create
    /// path's face: on an armed volume the gather arm first
    /// (`gather_mint_slot`), then the bounded parent-slot affinity
    /// policy with its overflow arm (one more rotor slot from the
    /// manager, up to `2 × M`); everywhere else the shared rotor verbatim.
    pub async fn pick_mint_slot_for(&self, volume_idx: usize, parent: Ino) -> u64 {
        if self.routing_width > 1 {
            if let Some(vol) = self.volumes.get(volume_idx) {
                if vol.slot_lease_armed() {
                    if let Some(slot) = self.gather_mint_slot(volume_idx, parent).await {
                        return slot;
                    }
                    let parent_slot = self.lease_parent_slot(volume_idx, Some(parent));
                    if let Some(choice) = vol.lease_mint_choice(parent_slot) {
                        if choice == crate::slot_lease_core::MintChoice::Overflow {
                            match vol.lease_mint_overflow().await {
                                Ok(Some(slot)) => {
                                    if let Ok(r) = vol.routing_slot_of_forest(slot) {
                                        return u64::from(r);
                                    }
                                }
                                Ok(None) => {}
                                Err(e) => log::warn!(
                                    "volume {volume_idx}: the affinity-ceiling overflow ask \
                                     failed ({e}) — minting into the smallest rotor tree"
                                ),
                            }
                        }
                        if let Some(slot) = self.lease_choice_slot(volume_idx, choice) {
                            return slot;
                        }
                    }
                }
            }
        }
        self.pick_mint_slot(volume_idx)
    }

    /// PR VL5b: mint one fresh ino on `volume_idx` in `mint` (a slot the
    /// caller picked via [`Self::pick_mint_slot`] and gated) — from the
    /// volume's native watermark when `mint` is its legacy keyspace,
    /// from the slot's travelling guest cursor otherwise. Returns
    /// `(effective local key ino, global ino)`.
    pub fn allocate_local_ino_in_slot(&self, volume_idx: usize, mint: u64) -> Result<(Ino, Ino)> {
        if self.routing_width <= 1 {
            let local = self.volumes[0].allocate_ino();
            return Ok((local, local));
        }
        if self.legacy_slot[volume_idx] == Some(mint as u16) {
            let raw = self.volumes[volume_idx].allocate_ino();
            if raw >= GUEST_NS_BASE {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "volume {volume_idx} exhausted its native local-ino namespace \
                     ({raw:#x} ≥ 2^{GUEST_NS_SHIFT}) — the monotonic watermark crossed \
                     into the guest partition"
                )));
            }
            Ok((raw, make_global_ino_width(raw, mint, self.routing_width)))
        } else {
            let raw = self.volumes[volume_idx].allocate_guest_ino(mint as u16)?;
            Ok((
                guest_local_ino(mint as u16, raw),
                make_global_ino_width(raw, mint, self.routing_width),
            ))
        }
    }

    /// [`Self::pick_mint_slot`] + [`Self::allocate_local_ino_in_slot`] —
    /// the ungated convenience (control-plane and test surface; the
    /// FUSE-facing op paths pick-then-gate-then-mint explicitly).
    pub fn allocate_local_ino(&self, volume_idx: usize) -> Result<(Ino, Ino)> {
        let mint = self.pick_mint_slot(volume_idx);
        self.allocate_local_ino_in_slot(volume_idx, mint)
    }

    /// Reserve `count` consecutive fresh inos from ONE (volume, slot)
    /// cursor — the rung-13 **intent-supply** grant (KD-MW-13): the
    /// UPDATE-grant holder mints child inos from this range locally, and
    /// because the reservation IS a cursor advance the owner can never
    /// re-mint them within its incarnation (unused ones burn, §4.8; a
    /// dead era's supply is fenced on the wire — see
    /// [`crate::meta_ship::InoSupply`]). Returns `(first_global, stride,
    /// count)`: the routing width's slot encoding is affine in the local,
    /// asserted here rather than assumed.
    ///
    /// Placement note (stated, not hidden): the whole supply rides one
    /// slot of one owned volume — mint-spread and client-owned-slot
    /// placement for intent mints are PR-14's lever, not this rung's.
    pub async fn reserve_intent_supply(&self, parent: Ino, count: u32) -> Result<(u64, u64, u32)> {
        let count = count.max(1);
        let (parent_v_idx, _) = self.route_ino(parent);
        let target_v_idx = crate::meta_ship::constrain_mint_volume(
            self.pick_mint_volume(parent_v_idx).await,
            parent_v_idx,
        );
        self.check_volume_enabled(target_v_idx)?;
        let mint_slot = self.pick_mint_slot(target_v_idx);
        // The cursor advance rides the slot gate exactly as a create's
        // mint does (a flip mid-advance would strand the range on the
        // source's travelled cursor).
        let mut pass = SlotGatePass::default();
        self.slot_gate_extend_slots(&mut pass, &[mint_slot]).await;
        if self.routing_width <= 1 {
            // The in-RAM test constructor's W ≤ 1 identity: global == local.
            let first = self.volumes[target_v_idx].reserve_ino_range(u64::from(count));
            return Ok((first, 1, count));
        }
        let first_raw = if self.legacy_slot[target_v_idx] == Some(mint_slot as u16) {
            let raw = self.volumes[target_v_idx].reserve_ino_range(u64::from(count));
            if raw.saturating_add(u64::from(count)) >= GUEST_NS_BASE {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "volume {target_v_idx}: an intent-supply reservation of {count} would cross \
                     the native local-ino namespace into the guest partition \
                     (watermark {raw:#x})"
                )));
            }
            raw
        } else {
            self.volumes[target_v_idx]
                .reserve_guest_ino_range(mint_slot as u16, u64::from(count))?
        };
        let g0 = make_global_ino_width(first_raw, mint_slot, self.routing_width);
        let g1 = make_global_ino_width(first_raw + 1, mint_slot, self.routing_width);
        let stride = g1 - g0;
        let g_last = make_global_ino_width(
            first_raw + u64::from(count) - 1,
            mint_slot,
            self.routing_width,
        );
        if g_last != g0 + u64::from(count - 1) * stride {
            // Structurally impossible for the modular slot encoding —
            // refused loud rather than assumed (a non-affine range would
            // hand the client numbers that route elsewhere).
            return Err(crate::error::SqueezefsError::InvalidOperation(
                "intent-supply reservation: the global ino encoding is not affine over the \
                 reserved range (routing bug)"
                    .to_string(),
            ));
        }
        Ok((g0, stride, count))
    }

    // -----------------------------------------------------------------
    // PR VL5b §5.5.2a: the per-slot cutover gate — checked at mutating
    // op entry BEFORE any 4a `lock_many`; parked ops hold no DLM or
    // node locks. Gate parks are planned and tagged
    // (`meta_slot_gate_parked_commits`) — they NEVER escalate to the
    // `disabled_volumes` fail-stop lattice (the §5.5.2a carve-out; the
    // cutover task's own deadline aborts-and-retries instead).
    // -----------------------------------------------------------------

    /// Arm the gate for `slot` (migration start). Idempotent per slot.
    pub fn arm_slot_gate(&self, slot: u64) -> std::sync::Arc<slot_gate_core::SlotGate> {
        if let Some(g) = self.gates.map.read_sync(&slot, |_, v| v.clone()) {
            return g;
        }
        let gate = std::sync::Arc::new(slot_gate_core::SlotGate::new());
        match self.gates.map.insert_sync(slot, gate.clone()) {
            Ok(()) => {
                self.gates
                    .armed
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                gate
            }
            Err(_) => self
                .gates
                .map
                .read_sync(&slot, |_, v| v.clone())
                .expect("gate raced in"),
        }
    }

    /// Disarm `slot`'s gate (migration finished or aborted). Wakes every
    /// parked op.
    pub fn disarm_slot_gate(&self, slot: u64) {
        if let Some((_, gate)) = self.gates.map.remove_sync(&slot) {
            gate.reopen();
            self.gates
                .armed
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.gates.notify.notify_waiters();
    }

    /// Wake gate parkers (the cutover's reopen path — the gate handle
    /// itself is flipped by the engine).
    pub fn wake_slot_gate_waiters(&self) {
        self.gates.notify.notify_waiters();
    }

    /// Gate admission for an op's DECLARED ino set — call at op entry,
    /// before any lock. Fast path: one relaxed load when no gate was
    /// ever armed.
    pub(crate) async fn slot_gate_enter(&self, inos: &[Ino]) -> SlotGatePass {
        let mut pass = SlotGatePass::default();
        self.slot_gate_extend(&mut pass, inos).await;
        pass
    }

    /// Extend a pass with more inos — may PARK (the caller must hold no
    /// 4a/4b locks; already-entered slots never re-park, so a pass can
    /// never deadlock against its own gate).
    pub(crate) async fn slot_gate_extend(&self, pass: &mut SlotGatePass, inos: &[Ino]) -> bool {
        if self.gates.armed.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            return false;
        }
        let slots: Vec<u64> = inos.iter().map(|&i| self.slot_of_ino(i)).collect();
        self.slot_gate_extend_slots(pass, &slots).await
    }

    /// [`Self::slot_gate_extend`] by SLOT id (create also gates its mint
    /// slot, which is not derivable from an existing ino).
    /// Returns whether any admission PARKED (parks can span a flip, so
    /// callers must re-derive routes taken before the call).
    pub(crate) async fn slot_gate_extend_slots(
        &self,
        pass: &mut SlotGatePass,
        slots: &[u64],
    ) -> bool {
        if self.gates.armed.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            return false;
        }
        let mut parked = false;
        for &slot in slots {
            if pass.entered.iter().any(|(s, _)| *s == slot) {
                continue;
            }
            let Some(gate) = self.gates.map.read_sync(&slot, |_, v| v.clone()) else {
                continue;
            };
            loop {
                let notified = self.gates.notify.notified();
                if gate.try_enter() {
                    break;
                }
                parked = true;
                crate::fuse_client::METRICS
                    .meta_slot_gate_parked_commits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                notified.await;
            }
            pass.entered.push((slot, gate));
        }
        parked
    }

    /// The per-ino xattr value cap — the largest inline xattr value
    /// `ino`'s volume can hold (design-cow-kv-metadata §5.3): the
    /// record-value cap `min(65_536, node_size/4)` (§4.2). A *non-trait*
    /// capability accessor (the `Metadata` trait stays unchanged, §5.1);
    /// the layout inline-spill decision
    /// (`DataRouter::save_metadata_to_backend`) consults it, so a volume
    /// set with differing `node_size` spills per volume by serialized
    /// size.
    pub fn xattr_value_cap(&self, ino: Ino) -> usize {
        let (v_idx, _) = self.route_ino(ino);
        self.volumes[v_idx].xattr_value_cap()
    }

    /// **DLM S8**: the dentry half of [`Metadata::lookup`] —
    /// `(child global ino, S_IFMT bits)` with **no child `getattr`**.
    ///
    /// A *non-trait* capability (the `xattr_value_cap` precedent). It
    /// exists because a shipped `lookup`'s two participants can home on
    /// different owners: the parent's dentry belongs to the parent's
    /// volume, while the child's inode record may live on a volume another
    /// node owns and this one may not read (one node cache per volume). So
    /// the router composes `lookup` from this call plus a separately
    /// routed `getattr` — which is exactly the two steps the trait's
    /// `lookup` already performs internally, and it was never atomic
    /// (see [`Metadata::lookup`]'s implementation note on dropping the
    /// D-guard before the I-lock).
    pub async fn lookup_dentry(&self, parent: Ino, name: &str) -> Result<Option<(Ino, u32)>> {
        // PR 7b: a striped parent's name is read where it lives.
        if let Some(route) = self.stripe_route(parent, name).await? {
            let (sv, slocal) = self.route_ino(route.stripe);
            self.check_volume_enabled(sv)?;
            let _guard = self.volumes[sv]
                .dlm()
                .lock_dentry_shared(slocal, name)
                .await;
            return Ok(self
                .stripe_locate(parent, name, &route)
                .await?
                .map(|(_, child, ft)| (child, ft)));
        }
        let (v_idx, local_parent) = self.route_ino(parent);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx]
            .dlm()
            .lock_dentry_shared(local_parent, name)
            .await;
        self.find_dentry_routed(v_idx, local_parent, name).await
    }

    /// The trait `getattr`'s LOCAL body, hook-free (rung 12): the S10
    /// delegated serve reads the holder's OWN reader-revalidation view
    /// through this — the trait verb would consult the daemon verb
    /// router and, on an armed co-writer, route straight back into the
    /// delegated serve (the live-rig recursion the first fleet mount
    /// found: `fuse3-tpc` lane stack overflow). Same guard discipline as
    /// the trait body verbatim.
    pub async fn getattr_local(&self, ino: Ino) -> Result<Inode> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let mut inode = {
            let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_ino).await;
            self.read_inode_routed(v_idx, local_ino).await?
        };
        inode.ino = ino;
        // PR 7b (design §5.6.5): a STRIPED directory's `nlink`/times are
        // the fold over its stripes (each stripe's record is the exact
        // delta its own inserts wrote); the persist runs after the shared
        // guard dropped — it takes the exclusive one.
        if inode.mode & libc::S_IFMT == libc::S_IFDIR && self.volumes[v_idx].slot_lease_armed() {
            if let Some(map) = self.stripe_map(ino).await? {
                if let Some((mtime, ctime)) = self.fold_striped_attrs(&mut inode, &map).await? {
                    self.persist_striped_times(ino, mtime, ctime).await;
                }
            }
        }
        Ok(inode)
    }

    /// The **commit watermark** of `ino`'s volume (rung 12): the journal
    /// reservation frontier every committed transaction sits below — the
    /// S10 delegation grant's view-currency stamp.
    pub fn commit_watermark_of(&self, ino: Ino) -> u64 {
        let (v_idx, _) = self.route_ino(ino);
        self.volumes[v_idx].commit_watermark()
    }

    /// The **view watermark** of `ino`'s volume: the journal prefix this
    /// mount's RAM-authoritative view covers (a reader/co-writer's
    /// adopted checkpoint tail; the commit frontier itself on a write
    /// mount). `view ≥ grant` is the delegated serve's currency law.
    pub fn view_watermark_of(&self, ino: Ino) -> u64 {
        let (v_idx, _) = self.route_ino(ino);
        self.volumes[v_idx].view_watermark()
    }

    /// The trait `readdir`'s LOCAL body, hook-free (the [`Self::getattr_local`]
    /// twin — the delegated serve's dentry page read).
    pub async fn readdir_local(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>> {
        let (v_idx, local_dir) = self.route_ino(dir);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_dir).await;
        // `offset` is a readdir cookie; pages resume strictly after its
        // key suffix (design §5.1).
        Ok(self
            .readdir_page_held(v_idx, local_dir, offset, max)
            .await?
            .into_iter()
            .map(|(_cookie, entry)| entry)
            .collect())
    }

    /// The ONE directory pager both listing faces ride, under the caller's
    /// held per-volume shared inode guard. A non-writer of a FOREST volume
    /// (S5 reader, co-writer, probe) lists a child iff the child's volume
    /// HOLDS the child's slot tree — exactly the condition its `lookup`
    /// (dentry + `getattr(child)`) answers — so `readdir` never lists a
    /// name `lookup` refuses: the partial view such a mount serves between
    /// the writer's mint of a slot and its next publication is a consistent
    /// snapshot, adopted whole at the poll that names the slot
    /// (design-symmetric-metadata §5.3.4; `meta_kv_forest_reader_unpublished_children`
    /// counts the withheld names). Withheld entries are re-paged from the
    /// last cookie the volume returned, so a page never ends short of `max`
    /// while the directory has more; the shipped path — every write mount,
    /// every flat volume — pays one bool per call and takes the volume's
    /// page verbatim.
    async fn readdir_page_held(
        &self,
        v_idx: usize,
        local_dir: Ino,
        offset: u64,
        max: usize,
    ) -> Result<Vec<(u64, DirEntry)>> {
        // PR 7b (design §5.6.5): a STRIPED directory lists the K-way merge
        // of its stripes (the stripe map's own marker entries never list —
        // no user name can start with NUL); an ordinary directory on an
        // armed volume drops a marker only if one exists, which the codec
        // makes impossible outside a striped directory.
        if self.volumes[v_idx].slot_lease_armed() {
            let dir = self.make_global_ino(local_dir, v_idx);
            if let Some(map) = self.stripe_map(dir).await? {
                return self.readdir_striped_page(&map, offset, max).await;
            }
        }
        if !self
            .volumes
            .iter()
            .any(|v| v.filters_unpublished_children())
        {
            let page = self.volumes[v_idx]
                .readdir_page(local_dir, offset, max)
                .await?;
            if self.volumes[v_idx].slot_lease_armed() {
                return Ok(page
                    .into_iter()
                    .filter(|(_, e)| !dir_stripe::is_marker_name(&e.name))
                    .collect());
            }
            return Ok(page);
        }
        let mut out: Vec<(u64, DirEntry)> = Vec::new();
        let mut cursor = offset;
        while out.len() < max {
            let want = max - out.len();
            let page = self.volumes[v_idx]
                .readdir_page(local_dir, cursor, want)
                .await?;
            let short = page.len() < want;
            let Some((last, _)) = page.last() else {
                break;
            };
            cursor = *last;
            for (cookie, entry) in page {
                let (child_v, child_local) = self.route_ino(entry.ino);
                if self.volumes[child_v].holds_slot_of(child_local)
                    && !dir_stripe::is_marker_name(&entry.name)
                {
                    out.push((cookie, entry));
                }
            }
            if short {
                break;
            }
        }
        Ok(out)
    }

    /// One cookie-paged readdir step against `dir`'s volume (design
    /// §5.1): pages of at most `max` `(resume_cookie, entry)` pairs, each
    /// entry paired with its resume cookie
    /// (`3 + ((hash54 << 8) | coll_seq)`). Dentry child inos are stored
    /// global, so no mapping. Takes the same per-volume shared inode
    /// guard as the trait `readdir`.
    pub async fn readdir_stream(
        &self,
        dir: Ino,
        offset: u64,
        max: usize,
    ) -> Result<Vec<(u64, DirEntry)>> {
        // Rung 13 — the OQ-2 read gate's LOCAL face (live finding: the
        // FUSE readdir handler rides THIS non-trait pager, not the trait
        // readdir, so an ungated stream could serve a directory page
        // missing a foreign holder's un-flushed intents). One relaxed
        // load when no delegation host is armed.
        crate::meta_ship::deleg_read_gate(self, dir).await;
        let (v_idx, local_dir) = self.route_ino(dir);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_dir).await;
        self.readdir_page_held(v_idx, local_dir, offset, max).await
    }

    /// [`Metadata::create_with_rdev`] with an INITIAL SIZE committed in
    /// the SAME create transaction (POSIX-3). A *non-trait* capability
    /// (the `Metadata` trait stays unchanged, §5.1 — the
    /// `xattr_value_cap` precedent): the only caller is the FUSE
    /// `symlink` handler, which must commit `size = strlen(target)`
    /// durably. Patching the size into the reply and the daemon attr
    /// cache alone made the first post-TTL `lstat()` report `st_size ==
    /// 0` (the size-coherency repair in `get_attr_internal` is
    /// regular-files-only), so tools that size a `readlink()` buffer
    /// from `st_size` recorded empty targets. Everything else about the
    /// create (routing, gating, locking, setgid inheritance, the
    /// one-whole-tx journal entry) is identical.
    pub async fn create_with_rdev_size(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        initial_size: u64,
    ) -> Result<Inode> {
        self.create_with_rdev_preset(parent, name, mode, uid, gid, rdev, initial_size, None)
            .await
    }

    /// [`Self::create_with_rdev_size`] with an optional **intent preset**
    /// (rung 13, KD-MW-13): the child's GLOBAL ino was pre-supplied to an
    /// UPDATE-grant holder (the owner's own cursor reservation, so the
    /// number can never collide within this incarnation) and the times
    /// are the client's mint instant. The preset arm skips the mint
    /// picks (the ino's own route IS the target) and answers a REPLAYED
    /// apply idempotently: an existing dentry naming exactly the preset
    /// ino is the op's own earlier apply (the witness-by-construction the
    /// explicit ino buys), never an EEXIST.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_rdev_preset(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        initial_size: u64,
        preset: Option<IntentCreatePreset>,
    ) -> Result<Inode> {
        let made = self
            .create_with_rdev_preset_guarded(
                parent,
                name,
                mode,
                uid,
                gid,
                rdev,
                initial_size,
                preset,
            )
            .await?;
        // PR 7b: the `-o stripe_dirs` flip of the directory this mkdir just
        // named runs HERE — after the create's 4a guards, its slot gate and
        // its delegation permit have all dropped. The flip takes its own
        // ONE canonical guard set on the same striped table; run while the
        // mkdir's `I{parent}` was still held it was a second acquisition,
        // and a stripe collision parked it for ever (PR 10 review, Issue
        // 28). Never the mkdir's error (the directory exists; the automatic
        // trigger can still flip it later).
        if (mode & libc::S_IFMT) == libc::S_IFDIR {
            self.stripe_at_mkdir(made.ino, parent).await;
        }
        Ok(made)
    }

    /// [`Self::create_with_rdev_preset`] under the op's guards — everything
    /// but the `-o stripe_dirs` flip, which the wrapper runs once these
    /// have dropped.
    #[allow(clippy::too_many_arguments)]
    async fn create_with_rdev_preset_guarded(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        initial_size: u64,
        preset: Option<IntentCreatePreset>,
    ) -> Result<Inode> {
        // PR 7b (design §5.6.5): a name in a STRIPED directory is keyed
        // under its stripe — the op below runs verbatim with the stripe as
        // its parent; the directory itself is the `..`/memo parent and the
        // `-o stripe_dirs` mkdir's subject. A preset mint is an intent
        // apply into the owner's own directory (the S10 delegation plane)
        // and never re-routes: the two planes do not compose today, so a
        // preset into a STRIPED parent is refused loud (Issue 20a) rather
        // than inserted into the directory's own tree.
        let logical_parent = parent;
        let striped = self.stripe_route(parent, name).await?;
        if preset.is_some() && striped.is_some() {
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "a preset (delegated) create into STRIPED directory {parent} is not composed \
                 (symmetric PR 7b × S10 delegation): the stripe is the key parent and a \
                 delegation names the directory"
            )));
        }
        let parent = match &striped {
            Some(route) => {
                self.stripe_insert_parent(logical_parent, name, route)
                    .await?
            }
            None => parent,
        };
        // S10 coherence law (rung 12) — BEFORE any 4a acquisition, like
        // the cutover gate below: a create mutates the parent's dentry
        // set + times, so every outstanding delegation on the parent is
        // recalled (acked or expired-dead) before this apply, and no new
        // grant can issue into the window while the permit is held. One
        // relaxed load on every mount without a delegation host.
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[parent]).await;
        // §5.5.2a cutover gate: BEFORE any 4a acquisition — a parked
        // create holds nothing. Routes are derived AFTER admission: a
        // park can span a flip, and held entries pin the map (the drain
        // waits on us) — so post-gate routes are stable for the op.
        let mut _gate = self.slot_gate_enter(&[parent]).await;
        let (parent_v_idx, local_parent) = self.route_ino(parent);
        self.check_volume_enabled(parent_v_idx)?;
        let is_dir = (mode & libc::S_IFMT) == libc::S_IFDIR;
        // DLM S8 (spec §6.10 R4 + §6.2 items 2/3/4): the placement engine
        // picks by health and balance; ownership then constrains the pick
        // to a volume THIS node owns, because a volume's journal ring,
        // extent bitmap and root ledger have exactly one appender. Unarmed
        // — every mount that ships today — this is one relaxed load and
        // the pick verbatim. A PRESET ino routes itself: the supply was
        // reserved on an owned volume's cursor at grant time.
        let target_v_idx = match &preset {
            Some(p) => self.route_ino(p.global_ino).0,
            None => crate::meta_ship::constrain_mint_volume(
                self.pick_mint_volume(parent_v_idx).await,
                parent_v_idx,
            ),
        };
        self.check_volume_enabled(target_v_idx)?;
        // The mint slot is a touched slot too (the new inode record
        // lands in its keyspace) — still before any 4a lock. The rotor
        // picks it HERE so the gate covers the exact slot the mint will
        // use (design-dynamic-meta-routing §5.4). A park here can span
        // a flip: re-derive the parent's route after.
        let mint_slot = match &preset {
            Some(p) => self.slot_of_ino(p.global_ino),
            None => self.pick_mint_slot_for(target_v_idx, parent).await,
        };
        {
            if self.slot_gate_extend_slots(&mut _gate, &[mint_slot]).await
                && self.route_ino(parent) != (parent_v_idx, local_parent)
            {
                // The parent's slot flipped while we parked on the mint
                // gate — surface a retryable error (EAGAIN class) rather
                // than committing through stale routes. In practice the
                // parent's slot is the armed one and we already held its
                // entry, so this is defensive, not a hot path.
                return Err(crate::error::SqueezefsError::Io(
                    std::io::Error::from_raw_os_error(libc::EAGAIN),
                ));
            }
        }

        // Directories need the EXCLUSIVE parent lock (parent nlink RMW).
        // Regular creates take a SHARED parent lock: they only update the
        // parent's mtime/ctime (never nlink/mode), so same-dir regular
        // creates run concurrently, while the shared lock still serializes
        // against any exclusive parent mutator (mkdir/setattr/unlink/
        // rename) — design §3.8.
        let parent_mode = if is_dir {
            dlm::LockMode::Exclusive
        } else {
            dlm::LockMode::Shared
        };
        // PR M7 (Issue 13): the op's guard set travels with each commit
        // (cloned per fragment for the cross-volume shape). ONE canonical
        // `lock_many` (I before D — the order the two single takes had);
        // a foreign parent's keys travel to its holder under the armed
        // plane (symmetric PR 6).
        let mut scope = None;
        let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(
            self.lock_many_leased(
                parent_v_idx,
                &mut scope,
                &[(local_parent, parent_mode)],
                &[(local_parent, name, dlm::LockMode::Exclusive)],
            )
            .await?,
        );
        // PR 7b: a stripe an `rmdir`'s intent marked dying refuses the
        // insert under the guard the mark took (R26's closer).
        if striped.is_some() {
            self.refuse_dying_parent(parent).await?;
        }

        // The preset REPLAY tiebreak (under the dentry guard): a dentry
        // already naming exactly the preset ino is this op's own earlier
        // apply — answer its record rather than EEXIST (the owner-failover
        // at-least-once shape; a dentry naming a DIFFERENT ino stays the
        // genuine EEXIST the arms below refuse).
        if let Some(p) = &preset {
            if let Some((existing, _)) = self
                .find_dentry_routed(parent_v_idx, local_parent, name)
                .await?
            {
                if existing == p.global_ino {
                    let (child_v, child_local) = self.route_ino(existing);
                    let mut inode = self.read_inode_routed(child_v, child_local).await?;
                    inode.ino = existing;
                    return Ok(inode);
                }
                return Err(crate::error::SqueezefsError::already_exists(
                    "File already exists",
                ));
            }
        }
        let ts_override = preset.as_ref().map(|p| p.ts_ns);

        // Symmetric PR 6 (§5.6 — the D1 shape): the parent's slot is
        // ANOTHER appender's ⇒ one cross-owner transaction — the child in
        // the creator's rotor slot (the mint policy never followed the
        // foreign parent) with the intent as ONE entry in the creator's
        // ring, the `InsertDentry` shipped to the holder. A preset mint
        // (an UPDATE-grant holder's flush, applied by the owner) is the
        // owner's own directory by construction and never reaches here.
        if preset.is_none() && self.spans_foreign_slot(&[parent]) {
            if striped.is_some() {
                // The metanode cost, spread: one ship to the name's stripe
                // holder (`dir_stripe_ships`).
                dir_stripe::DIR_STRIPE_SHIPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let made = self
                .create_in_foreign_directory(
                    parent,
                    logical_parent,
                    parent_v_idx,
                    local_parent,
                    target_v_idx,
                    mint_slot,
                    name,
                    mode,
                    uid,
                    gid,
                    rdev,
                    initial_size,
                    guards,
                )
                .await;
            return match made {
                Ok(m) => Ok(m),
                // The holder's witness refusal of an insert into a dying
                // stripe is the plan's `EEXIST`; the op's errno is `ENOENT`
                // (one record read, the error path only).
                Err(e) if striped.is_some() => Err(self.dying_parent_errno(parent, e).await),
                Err(e) => Err(e),
            };
        }

        if parent_v_idx == target_v_idx {
            // Same-volume create: ONE whole-tx journal entry with the
            // routed semantics (design §4.4). The routed layer allocates
            // the (effective local, global) pair — PR VL5b: mints ride
            // the volume's mint slot's keyspace/cursor, so the backend
            // stays keyspace-agnostic. (A failed create burns the ino —
            // the standing §4.8 monotonic-allocation law.) A preset pair
            // was allocated at grant time and routes itself.
            let (new_local, new_global) = match &preset {
                Some(p) => (self.route_ino(p.global_ino).1, p.global_ino),
                None => self.allocate_local_ino_in_slot(target_v_idx, mint_slot)?,
            };
            let be = &self.volumes[target_v_idx];
            let out = be
                .routed_create_local(
                    local_parent,
                    name,
                    mode,
                    uid,
                    gid,
                    rdev,
                    initial_size,
                    new_local,
                    new_global,
                    ts_override,
                    guards,
                )
                .await;
            if out.is_err() {
                self.mirror_volume_failure(target_v_idx);
            }
            let made = out?;
            if is_dir && striped.is_some() {
                self.note_dir_parent(made.ino, logical_parent, name);
            }
            Ok(made)
        } else {
            // Cross-volume create: mutate each volume only through its own
            // whole-tx commit (mixed-volume sets stripe directories by
            // health, §4.9).
            if self
                .find_dentry_routed(parent_v_idx, local_parent, name)
                .await?
                .is_some()
            {
                return Err(crate::error::SqueezefsError::already_exists(
                    "File already exists",
                ));
            }

            let parent_inode = self.read_inode_routed(parent_v_idx, local_parent).await?;
            let mut final_gid = gid;
            let mut final_mode = mode;
            if (parent_inode.mode & libc::S_ISGID) != 0 {
                final_gid = parent_inode.gid;
                if (mode & libc::S_IFMT) == libc::S_IFDIR {
                    final_mode |= libc::S_ISGID;
                }
            }

            let is_dir_flag = is_dir;
            // Target side: mint the child inode record (PR VL5b: the
            // routed allocation rides the GATED mint slot's
            // keyspace/cursor; a preset pair routes itself).
            let (new_local_ino, global_child_ino) = match &preset {
                Some(p) => (self.route_ino(p.global_ino).1, p.global_ino),
                None => self.allocate_local_ino_in_slot(target_v_idx, mint_slot)?,
            };
            let target_be = &self.volumes[target_v_idx];
            let minted = target_be
                .routed_mint_inode(
                    new_local_ino,
                    final_mode,
                    uid,
                    final_gid,
                    rdev,
                    initial_size,
                    ts_override,
                    guards.clone(),
                )
                .await;
            if minted.is_err() {
                self.mirror_volume_failure(target_v_idx);
            }
            let v = minted?;
            let child_inode = Inode {
                ino: new_local_ino,
                mode: v.mode,
                uid: v.uid,
                gid: v.gid,
                size: v.size,
                nlink: v.nlink,
                atime: v.atime,
                mtime: v.mtime,
                ctime: v.ctime,
                flags: v.flags,
                rdev: v.rdev,
            };

            // Parent side: the dentry (global child ino) + parent update.
            let update = if is_dir_flag {
                kv::backend::RoutedParentUpdate::ExclusiveTimesBump
            } else {
                kv::backend::RoutedParentUpdate::SharedTimes
            };
            let out = self.volumes[parent_v_idx]
                .routed_add_dentry(
                    local_parent,
                    name,
                    global_child_ino,
                    final_mode & libc::S_IFMT,
                    update,
                    guards.clone(),
                )
                .await;
            if out.is_err() {
                self.mirror_volume_failure(parent_v_idx);
            }
            out?;

            if is_dir_flag {
                self.note_dir_parent(global_child_ino, logical_parent, name);
            }
            Ok(Inode {
                ino: global_child_ino,
                ..child_inode
            })
        }
    }

    /// **Create in a foreign directory** (symmetric PR 6, design §5.6's
    /// sequence diagram): plan under the creator's guards (the EEXIST
    /// probe and the setgid inputs read from the parent's record —
    /// exact in-process; PR 5's tokens are the wire's exactness), mint
    /// the child in the creator's ROTOR slot, then ONE intent:
    /// `[CreateInode @ own slot, InsertDentry @ the parent's holder]` —
    /// `tx0` = the child record + the intent in the creator's ring, the
    /// insert shipped, the holder's reply after its lane, the intent
    /// retired. A holder that finds the name taken refuses the insert
    /// under its witness: the create answers `EEXIST` and the minted
    /// child is destroyed before the retirement.
    #[allow(clippy::too_many_arguments)]
    async fn create_in_foreign_directory(
        &self,
        parent: Ino,
        logical_parent: Ino,
        parent_v_idx: usize,
        local_parent: Ino,
        target_v_idx: usize,
        mint_slot: u64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        initial_size: u64,
        guards: std::sync::Arc<[dlm::DlmGuard]>,
    ) -> Result<Inode> {
        if self
            .find_dentry_routed(parent_v_idx, local_parent, name)
            .await?
            .is_some()
        {
            return Err(crate::error::SqueezefsError::already_exists(
                "File already exists",
            ));
        }
        let parent_inode = self.read_inode_routed(parent_v_idx, local_parent).await?;
        let is_dir = (mode & libc::S_IFMT) == libc::S_IFDIR;
        let mut final_gid = gid;
        let mut final_mode = mode;
        if (parent_inode.mode & libc::S_ISGID) != 0 {
            final_gid = parent_inode.gid;
            if is_dir {
                final_mode |= libc::S_ISGID;
            }
        }
        let (_new_local, child) = self.allocate_local_ino_in_slot(target_v_idx, mint_slot)?;
        let update = if is_dir {
            kv::backend::RoutedParentUpdate::ExclusiveTimesBump
        } else {
            kv::backend::RoutedParentUpdate::SharedTimes
        };
        let plan = crossvol_tx::XvPlan {
            op: crossvol_tx::XvOp::Create,
            steps: vec![
                crossvol_tx::XvStep::CreateInode {
                    ino: child,
                    mode: final_mode,
                    uid,
                    gid: final_gid,
                    rdev,
                    size: initial_size,
                    ts_ns: kv::backend::KvMetaBackend::now_ns_pub(),
                },
                crossvol_tx::XvStep::InsertDentry {
                    parent,
                    name: name.to_string(),
                    child,
                    ft_bits: final_mode & libc::S_IFMT,
                    parent_update: crossvol_tx::parent_update_code(update),
                },
            ],
        };
        let done = crossvol_tx::execute(self, &plan, guards).await?;
        let v = done.inode(0).cloned().ok_or_else(|| {
            crate::error::SqueezefsError::InvalidOperation(
                "cross-owner create: the mint step produced no record".into(),
            )
        })?;
        if is_dir {
            self.note_dir_parent(child, logical_parent, name);
        }
        Ok(Inode {
            ino: child,
            mode: v.mode,
            uid: v.uid,
            gid: v.gid,
            size: v.size,
            nlink: v.nlink,
            atime: v.atime,
            mtime: v.mtime,
            ctime: v.ctime,
            flags: v.flags,
            rdev: v.rdev,
        })
    }
    /// The rename proper, after the wrapper's flag screen and — on an
    /// armed mount, for a DIRECTORY source — under the set-wide
    /// `dir_rename` lease (`dir_lock_held`). `Ok(false)` = the source
    /// turned out to be a directory under the guards and the lock is not
    /// held: the wrapper takes it and re-enters (the lock is OUTERMOST —
    /// taken after a 4a guard it would cycle with a peer's shipped step).
    #[allow(clippy::too_many_arguments)]
    async fn rename_body(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
        flags: u32,
        dir_lock_held: bool,
        // PR 7b: `new_parent` is a STRIPE — the dying check runs under the
        // guards (R26's closer at the rename's insert).
        dest_is_stripe: bool,
    ) -> Result<bool> {
        // S10 coherence law (rung 12): both parents' dentry sets mutate;
        // the moved child's ctime moves, and a rename-over destroys the
        // target — both children may be delegated objects, so they join
        // the recall set when delegations are outstanding (advisory
        // resolution; the held permit's grant decline is the backstop).
        let mut deleg_set = vec![old_parent, new_parent];
        if crate::meta_ship::deleg_gate_wants_children(self) {
            for (parent, name) in [(old_parent, old_name), (new_parent, new_name)] {
                if let Ok(Some((child, _))) = self.lookup_dentry(parent, name).await {
                    deleg_set.push(child);
                }
            }
        }
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &deleg_set).await;
        // §5.5.2a cutover gate — the deterministic G-VL-4 cross-slot-
        // rename case: BOTH parents' slots checked before any 4a
        // acquisition (and before route derivation — a park can span a
        // flip), so a rename spanning the migrating slot parks WHOLE,
        // holding zero guards. The children's slots join after phase-1
        // discovery (holding nothing — a park there is legal).
        let mut _gate = self.slot_gate_enter(&[old_parent, new_parent]).await;
        let (old_parent_v_idx, local_old_parent) = self.route_ino(old_parent);
        let (new_parent_v_idx, local_new_parent) = self.route_ino(new_parent);
        self.check_volume_enabled(old_parent_v_idx)?;
        self.check_volume_enabled(new_parent_v_idx)?;

        // RENAME_WHITEOUT (fstests generic/631, the overlayfs-upper
        // contract) mints a fresh char-0:0 inode in the old parent's
        // volume: its mint slot joins the gate BEFORE any 4a acquisition
        // (the create-path discipline — the rotor picks here so the gate
        // covers the exact slot the mint will use; a park here can span
        // a flip, so both parents' routes are re-verified after).
        let whiteout_mint_slot = if flags & libc::RENAME_WHITEOUT != 0 {
            let mint = self.pick_mint_slot(old_parent_v_idx);
            if self.slot_gate_extend_slots(&mut _gate, &[mint]).await
                && (self.route_ino(old_parent) != (old_parent_v_idx, local_old_parent)
                    || self.route_ino(new_parent) != (new_parent_v_idx, local_new_parent))
            {
                return Err(crate::error::SqueezefsError::Io(
                    std::io::Error::from_raw_os_error(libc::EAGAIN),
                ));
            }
            Some(mint)
        } else {
            None
        };

        // **The rename lock set** (the lock law of `src/stripe_locks.rs`;
        // PR 4 review round 2, Issue 1 — a SHIPPED, layout-independent
        // defect: the set named the two parents' I/D keys only, and the
        // moved child's `Delta` / the overwrite victim's `Put` were staged
        // under guards that never named `I{moved}` / `I{dest}`, so a
        // concurrent `set_layout_and_size` — which holds `I{ino}` and
        // stages a `Put` of the same key — CO-QUEUED with the rename in
        // one conveyor batch: the pass's same-key sentinel in debug, a
        // lost ctime in release). The unlink path's two-phase shape:
        // phase 1 discovers both children holding NOTHING; phase 2 takes
        // ONE canonical `lock_many` per volume over the parents, the
        // children and the two D keys — per-volume sets in ascending
        // volume order, each internally canonical (I before D, stripe-
        // deduped by `lock_many`) — and re-reads both dentries under the
        // guards, retrying the plan when either child moved.
        let (old_dentry_opt, new_dentry_opt, guards) = loop {
            let old_dentry_opt = self
                .find_dentry_routed(old_parent_v_idx, local_old_parent, old_name)
                .await?;
            let new_dentry_opt = self
                .find_dentry_routed(new_parent_v_idx, local_new_parent, new_name)
                .await?;
            // The discovered children's slots join the gate BEFORE any 4a
            // acquisition (holding nothing — a park is legal). A park can
            // span a flip: the parents' routes are re-verified.
            {
                let mut join = Vec::new();
                if let Some((c, _)) = old_dentry_opt {
                    join.push(c);
                }
                if let Some((c, _)) = new_dentry_opt {
                    join.push(c);
                }
                if self.slot_gate_extend(&mut _gate, &join).await
                    && (self.route_ino(old_parent) != (old_parent_v_idx, local_old_parent)
                        || self.route_ino(new_parent) != (new_parent_v_idx, local_new_parent))
                {
                    return Err(crate::error::SqueezefsError::Io(
                        std::io::Error::from_raw_os_error(libc::EAGAIN),
                    ));
                }
            }
            // Per-volume lock sets: `(volume, I keys, D keys)`.
            let mut per_volume: std::collections::BTreeMap<
                usize,
                (Vec<(Ino, dlm::LockMode)>, Vec<(Ino, &str, dlm::LockMode)>),
            > = std::collections::BTreeMap::new();
            per_volume
                .entry(old_parent_v_idx)
                .or_default()
                .0
                .push((local_old_parent, dlm::LockMode::Exclusive));
            per_volume.entry(old_parent_v_idx).or_default().1.push((
                local_old_parent,
                old_name,
                dlm::LockMode::Exclusive,
            ));
            per_volume
                .entry(new_parent_v_idx)
                .or_default()
                .0
                .push((local_new_parent, dlm::LockMode::Exclusive));
            per_volume.entry(new_parent_v_idx).or_default().1.push((
                local_new_parent,
                new_name,
                dlm::LockMode::Exclusive,
            ));
            for child in old_dentry_opt
                .iter()
                .chain(new_dentry_opt.iter())
                .map(|(c, _)| *c)
            {
                let (v, l) = self.route_ino(child);
                self.check_volume_enabled(v)?;
                per_volume
                    .entry(v)
                    .or_default()
                    .0
                    .push((l, dlm::LockMode::Exclusive));
            }
            let mut guards = Vec::new();
            let mut scope = None;
            for (v_idx, (mut inos, dents)) in per_volume {
                inos.sort_unstable_by_key(|(l, _)| *l);
                inos.dedup_by_key(|(l, _)| *l);
                guards.extend(
                    self.lock_many_leased(v_idx, &mut scope, &inos, &dents)
                        .await?,
                );
            }
            // Revalidate under the guards: both dentries as discovered.
            let old_now = self
                .find_dentry_routed(old_parent_v_idx, local_old_parent, old_name)
                .await?;
            let new_now = self
                .find_dentry_routed(new_parent_v_idx, local_new_parent, new_name)
                .await?;
            if old_now.map(|(c, _)| c) == old_dentry_opt.map(|(c, _)| c)
                && new_now.map(|(c, _)| c) == new_dentry_opt.map(|(c, _)| c)
            {
                // PR M7 (Issue 13): Arc the op's guard set — the same-
                // volume one-tx shape takes it once; the cross-volume
                // fragments clone it per sequential commit.
                let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(guards);
                break (old_now, new_now, guards);
            }
            // A child moved under us — rediscover (guards dropped here).
        };
        // PR 7b: a destination stripe an `rmdir`'s intent marked dying
        // refuses the insert under the guards (R26's closer).
        if dest_is_stripe {
            self.refuse_dying_parent(new_parent).await?;
        }

        // **§5.4a M1**, with the PLURAL participant set the owner side
        // already uses (`service.rs`'s loop over both `(parent, name)`
        // pairs): the moved ino, the overwrite victim, and — under
        // `RENAME_EXCHANGE` — both participants. The parents themselves
        // are named, so `route_verb` covered them; these are the
        // discovered ones no router can see.
        {
            let mut discovered = Vec::new();
            if let Some((c, _)) = old_dentry_opt {
                discovered.push(c);
            }
            if let Some((c, _)) = new_dentry_opt {
                discovered.push(c);
            }
            self.refuse_cross_owner_participants(crate::meta_ship::MetaVerb::Rename, &discovered)?;
        }

        // Symmetric PR 6 (§5.6.4, KD-SYM-14): on an armed mount EVERY
        // rename whose source is a DIRECTORY runs under the set-wide
        // lease — same-slot ones too — and checks ancestry on exact data
        // before any step: the target's parent chain must not reach the
        // moved directory (an exchange checks both directions).
        let armed = self.volumes[old_parent_v_idx].slot_lease_armed();
        if armed {
            let dirs: Vec<(Ino, Ino)> = [
                old_dentry_opt.map(|(c, ft)| (c, ft, new_parent)),
                (flags & libc::RENAME_EXCHANGE != 0)
                    .then_some(new_dentry_opt.map(|(c, ft)| (c, ft, old_parent)))
                    .flatten(),
            ]
            .into_iter()
            .flatten()
            .filter(|(_, ft, _)| *ft == libc::S_IFDIR)
            .map(|(c, _, into)| (c, into))
            .collect();
            if !dirs.is_empty() {
                if !dir_lock_held {
                    return Ok(false);
                }
                for (moved, into) in dirs {
                    self.refuse_rename_into_own_subtree(moved, into).await?;
                }
            }
        }

        // Symmetric PR 6: any participant in another appender's slot takes
        // the transaction path — its steps route by slot.
        let foreign = armed && {
            let mut all = vec![old_parent, new_parent];
            all.extend(
                old_dentry_opt
                    .iter()
                    .chain(new_dentry_opt.iter())
                    .map(|(c, _)| *c),
            );
            self.spans_foreign_slot(&all)
        };
        if old_parent_v_idx == new_parent_v_idx && !foreign {
            // Same-volume rename: dentry surgery + dir-move nlink shifts +
            // parent Δtimes + local-dest accounting + the moved inode's
            // Δctime as ONE whole-tx entry (PR M6 D4.b); a remote
            // destination inode is settled first (ENOTEMPTY aborts before
            // any surgery) — check-then-mutate order — and a remote
            // moved/exchanged inode gets its ctime as a per-volume
            // fragment after the surgery (cross-volume renames were never
            // transactional across volumes).
            let be = &self.volumes[old_parent_v_idx];
            let (src_local, src_remote) = match old_dentry_opt {
                Some((src_global, _ft)) => {
                    let (v, l) = self.route_ino(src_global);
                    if v == old_parent_v_idx {
                        (Some(l), None)
                    } else {
                        (None, Some((v, l)))
                    }
                }
                None => (None, None),
            };
            let mut dest_local = None;
            let mut dest_remote = None;
            if flags & libc::RENAME_EXCHANGE != 0 {
                if let Some((dest_global, _ft)) = new_dentry_opt {
                    let (v, l) = self.route_ino(dest_global);
                    if v == old_parent_v_idx {
                        dest_local = Some(l);
                    } else {
                        dest_remote = Some((v, l));
                    }
                }
            } else if let Some((dest_global, _ft)) = new_dentry_opt {
                if flags & libc::RENAME_NOREPLACE != 0 {
                    return Err(crate::error::SqueezefsError::Io(
                        std::io::Error::from_raw_os_error(libc::EEXIST),
                    ));
                }
                let (dest_v_idx, local_dest) = self.route_ino(dest_global);
                if dest_v_idx == old_parent_v_idx {
                    dest_local = Some(local_dest);
                } else {
                    self.dest_replace_routed(dest_v_idx, local_dest, guards.clone())
                        .await?;
                }
            }
            // RENAME_WHITEOUT: pre-allocate the whiteout's (local,
            // global) pair — minted in the old parent's volume, rides
            // the same whole-tx entry. (A failed rename burns the ino —
            // the standing §4.8 monotonic-allocation law, as at create.)
            let whiteout = match whiteout_mint_slot {
                Some(mint) => Some(self.allocate_local_ino_in_slot(old_parent_v_idx, mint)?),
                None => None,
            };
            let out = be
                .routed_rename_local(
                    local_old_parent,
                    old_name,
                    local_new_parent,
                    new_name,
                    flags,
                    src_local,
                    dest_local,
                    whiteout,
                    guards.clone(),
                )
                .await;
            if out.is_err() {
                self.mirror_volume_failure(old_parent_v_idx);
            }
            out?;
            // Remote moved/exchanged inode ctime fragments.
            if let Some((v, l)) = src_remote {
                self.touch_ctime_routed(v, l, guards.clone()).await?;
            }
            if let Some((v, l)) = dest_remote {
                self.touch_ctime_routed(v, l, guards.clone()).await?;
            }
            self.note_renamed_dir_parents(
                old_dentry_opt,
                new_dentry_opt,
                flags,
                (old_parent, old_name),
                (new_parent, new_name),
            );
            Ok(true)
        } else if flags & libc::RENAME_EXCHANGE != 0 {
            let (old_child, old_ft) = old_dentry_opt.ok_or_else(|| {
                crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(libc::ENOENT))
            })?;
            let (new_child, new_ft) = new_dentry_opt.ok_or_else(|| {
                crate::error::SqueezefsError::Io(std::io::Error::from_raw_os_error(libc::ENOENT))
            })?;

            // Remove both, insert swapped — ONE cross-volume transaction
            // (DUR-7, §4.10a; the fragment sequence this replaces could
            // lose BOTH names to a crash between its first two commits).
            // PR M6 D4.b: each dentry step carries its parent's time
            // update; the swapped inodes' ctimes are their own steps.
            let now = kv::backend::KvMetaBackend::now_ns_pub();
            let exclusive =
                crossvol_tx::parent_update_code(kv::backend::RoutedParentUpdate::ExclusiveTimes);
            let plan = crossvol_tx::XvPlan {
                op: crossvol_tx::XvOp::Exchange,
                steps: vec![
                    crossvol_tx::XvStep::RemoveDentry {
                        parent: old_parent,
                        name: old_name.to_string(),
                        expect_child: old_child,
                        parent_update: exclusive,
                    },
                    crossvol_tx::XvStep::RemoveDentry {
                        parent: new_parent,
                        name: new_name.to_string(),
                        expect_child: new_child,
                        parent_update: exclusive,
                    },
                    crossvol_tx::XvStep::InsertDentry {
                        parent: new_parent,
                        name: new_name.to_string(),
                        child: old_child,
                        ft_bits: old_ft,
                        parent_update: exclusive,
                    },
                    crossvol_tx::XvStep::InsertDentry {
                        parent: old_parent,
                        name: old_name.to_string(),
                        child: new_child,
                        ft_bits: new_ft,
                        parent_update: exclusive,
                    },
                    crossvol_tx::XvStep::TouchCtime {
                        ino: old_child,
                        ctime: now,
                    },
                    crossvol_tx::XvStep::TouchCtime {
                        ino: new_child,
                        ctime: now,
                    },
                ],
            };
            crossvol_tx::execute(self, &plan, guards).await?;
            self.note_renamed_dir_parents(
                old_dentry_opt,
                new_dentry_opt,
                flags,
                (old_parent, old_name),
                (new_parent, new_name),
            );
            Ok(true)
        } else {
            if flags & libc::RENAME_NOREPLACE != 0 && new_dentry_opt.is_some() {
                return Err(crate::error::SqueezefsError::Io(
                    std::io::Error::from_raw_os_error(libc::EEXIST),
                ));
            }

            if let Some((old_child, old_ft)) = old_dentry_opt {
                let is_dir = old_ft == libc::S_IFDIR;
                // **Cross-parent rename is ONE cross-volume transaction**
                // (DUR-7, design-cow-kv-metadata §4.10a). The step order is
                // the fragment order this replaces, verbatim — parent
                // `nlink` shifts, destination settlement, source removal,
                // destination insert, the moved inode's ctime, then the
                // whiteout — so no user-visible sequencing moves; what
                // changes is that a crash no longer drifts a parent's link
                // count permanently (`rmdir` then either succeeded with
                // children present or refused forever).
                let now = kv::backend::KvMetaBackend::now_ns_pub();
                let exclusive = crossvol_tx::parent_update_code(
                    kv::backend::RoutedParentUpdate::ExclusiveTimes,
                );
                let mut steps: Vec<crossvol_tx::XvStep> = Vec::new();

                if is_dir && old_parent != new_parent {
                    // Directory move across parents: the nlink shift on
                    // each side, with the POSIX-11 underflow guard applied
                    // HERE (planning) exactly as `routed_parent_nlink_delta`
                    // applied it at commit — a suppressed decrement is a
                    // counted bug signal and simply produces no step.
                    for (target, local, delta) in [
                        (old_parent, local_old_parent, -1i64),
                        (new_parent, local_new_parent, 1i64),
                    ] {
                        let (v_idx, _) = self.route_ino(target);
                        let Some(pv) = self.volumes[v_idx].read_inode_value_routed(local).await?
                        else {
                            // The v2/routed arms tolerated an unreadable
                            // parent here (best-effort); no step.
                            continue;
                        };
                        let post = if delta > 0 {
                            pv.nlink + 1
                        } else if pv.nlink > 2 {
                            pv.nlink - 1
                        } else {
                            crate::fuse_client::METRICS
                                .dir_nlink_underflows
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            log::warn!(
                                "directory nlink underflow guard fired on parent {local}: \
                                 nlink {} cannot absorb a {delta} decrement — the deficit \
                                 is permanent (POSIX-11; run `squeezefs fsck`)",
                                pv.nlink
                            );
                            continue;
                        };
                        steps.push(crossvol_tx::XvStep::SetNlink {
                            ino: target,
                            pre: pv.nlink,
                            post,
                            ctime: None,
                        });
                    }
                }

                // Destination replacement: settle its inode (the
                // ENOTEMPTY probe is validation and happens HERE, before
                // any step is durable — check-then-mutate, unchanged),
                // then remove its dentry.
                if let Some((dest_ino, _dest_ft)) = new_dentry_opt {
                    let (dest_v_idx, local_dest) = self.route_ino(dest_ino);
                    if let Some(dv) = self.volumes[dest_v_idx]
                        .read_inode_value_routed(local_dest)
                        .await?
                    {
                        let dest_is_dir = (dv.mode & libc::S_IFMT) == libc::S_IFDIR;
                        if dest_is_dir
                            && self.volumes[dest_v_idx].dir_has_entries(local_dest).await?
                        {
                            return Err(crate::error::SqueezefsError::Io(
                                std::io::Error::from_raw_os_error(libc::ENOTEMPTY),
                            ));
                        }
                        // Replaced directory ⇒ nlink 0 (the rmdir rule —
                        // generic/035); otherwise one link fewer.
                        let post = if dest_is_dir {
                            0
                        } else {
                            dv.nlink.saturating_sub(1)
                        };
                        steps.push(crossvol_tx::XvStep::SetNlink {
                            ino: dest_ino,
                            pre: dv.nlink,
                            post,
                            ctime: Some(now),
                        });
                    }
                    steps.push(crossvol_tx::XvStep::RemoveDentry {
                        parent: new_parent,
                        name: new_name.to_string(),
                        expect_child: dest_ino,
                        parent_update: exclusive,
                    });
                }

                steps.push(crossvol_tx::XvStep::RemoveDentry {
                    parent: old_parent,
                    name: old_name.to_string(),
                    expect_child: old_child,
                    parent_update: exclusive,
                });
                steps.push(crossvol_tx::XvStep::InsertDentry {
                    parent: new_parent,
                    name: new_name.to_string(),
                    child: old_child,
                    ft_bits: old_ft,
                    parent_update: exclusive,
                });
                // PR M6 D4.b: the moved inode's ctime.
                steps.push(crossvol_tx::XvStep::TouchCtime {
                    ino: old_child,
                    ctime: now,
                });
                // RENAME_WHITEOUT: the char-0:0 whiteout + its dentry at
                // the OLD name, inside the SAME transaction (the crash
                // window that left the rename done without its whiteout —
                // an overlayfs upper layer losing a deletion marker — is
                // gone; the ino is still burned on failure, the standing
                // §4.8 monotonic-allocation law).
                if let Some(mint) = whiteout_mint_slot {
                    let (_w_local, w_global) =
                        self.allocate_local_ino_in_slot(old_parent_v_idx, mint)?;
                    steps.push(crossvol_tx::XvStep::MintInode {
                        ino: w_global,
                        mode: libc::S_IFCHR,
                        uid: 0,
                        gid: 0,
                        rdev: 0,
                    });
                    steps.push(crossvol_tx::XvStep::InsertDentry {
                        parent: old_parent,
                        name: old_name.to_string(),
                        child: w_global,
                        ft_bits: libc::S_IFCHR,
                        parent_update: exclusive,
                    });
                }

                let plan = crossvol_tx::XvPlan {
                    op: crossvol_tx::XvOp::Rename,
                    steps,
                };
                crossvol_tx::execute(self, &plan, guards).await?;
                self.note_renamed_dir_parents(
                    old_dentry_opt,
                    None,
                    flags,
                    (old_parent, old_name),
                    (new_parent, new_name),
                );
                Ok(true)
            } else {
                Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Source dentry not found",
                )))
            }
        }
    }
}

#[async_trait::async_trait]
impl Metadata for RoutedMetaBackend {
    // Rung 9 (the S8 arm): every verb of this impl consults the daemon
    // verb-router hook (`meta_ship::daemon_verb_router`) at entry and
    // delegates to the installed `MetaShipRouter` when the participant's
    // volume has a FOREIGN owner — the co-writer daemon's mutations SHIP
    // instead of refusing at the write gate, and its reads are
    // owner-current (read-your-own-shipped-writes; S8 raw's honest RTT
    // cost, spec §6.10 R1 — S10's delegation is the recovery). The hook is
    // one relaxed load on every unarmed mount, and it lives HERE so no
    // call site can bypass it (the S8-b falsifier is "any un-routed local
    // commit"). Recursion-free by construction: the hook delegates exactly
    // when the router would answer `Ship`, so the router's Local arm only
    // executes when the hook answered `None` — with ONE deliberate
    // carve-out, `".."` lookups, which both layers keep local (the
    // reverse-dentry walk is not expressible on the wire until S10).
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode> {
        if name != ".." {
            if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[parent]) {
                return r.lookup(parent, name).await;
            }
        }
        // ".." — the FUSE_EXPORT_SUPPORT directory-handle reconnect path
        // (fstests generic/467): no parent pointer exists in the inode
        // record, so the parent resolves by reverse dentry scan — cold
        // and rare by construction (see
        // `KvMetaBackend::find_parent_of_child` for the priced cost
        // note). Loud NotFound when no dentry names the child (an
        // orphaned/racing-unlinked dir) — never a fabricated parent.
        if name == ".." {
            if parent == 1 {
                return self.getattr(1).await;
            }
            for (v_idx, vol) in self.volumes.iter().enumerate() {
                self.check_volume_enabled(v_idx)?;
                if let Some(local_p) = vol.find_parent_of_child(parent).await? {
                    let mut p = self.make_global_ino(local_p, v_idx);
                    // PR 7b: a child of a STRIPED directory is named by a
                    // stripe; its `..` is the directory the stripe belongs
                    // to (one more hop, on this reconnect path only).
                    if let Some(dir) = self.stripe_parent_dir(p).await? {
                        p = dir;
                    }
                    return self.getattr(p).await;
                }
            }
            return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no dentry names ino {parent} — cannot resolve \"..\""),
            )));
        }
        // Rung 13 — the OQ-2 read gate's LOCAL face: the owner's own
        // lookup under a directory with an outstanding foreign UPDATE
        // grant recalls it (forcing the holder's intent flush) BEFORE the
        // serve. One relaxed load when no delegation host is armed.
        crate::meta_ship::deleg_read_gate(self, parent).await;
        // PR 7b: a striped directory's name lives in its stripe (or in the
        // directory's own tree while the flip's migration runs — the
        // stripe wins where both hold it).
        if let Some(route) = self.stripe_route(parent, name).await? {
            let (key_parent, _) = self.route_ino(route.stripe);
            let _guard = self.volumes[key_parent]
                .dlm()
                .lock_dentry_shared(self.route_ino(route.stripe).1, name)
                .await;
            let found = self.stripe_locate(parent, name, &route).await?;
            drop(_guard);
            return match found {
                Some((_, child, _)) => self.getattr(child).await,
                None => Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("Dentry {} not found in striped parent {}", name, parent),
                ))),
            };
        }
        let (v_idx, local_parent) = self.route_ino(parent);
        self.check_volume_enabled(v_idx)?;
        // Drop the D-guard before getattr's I-lock (canonical class order —
        // holding a D-stripe while waiting on an I-stripe inverts the class
        // order: ABBA against unlink's held child-I under stripe
        // collisions). Snapshot semantics are unchanged — lookup→getattr
        // was never atomic.
        let child_ino = {
            let _guard = self.volumes[v_idx]
                .dlm()
                .lock_dentry_shared(local_parent, name)
                .await;
            match self.find_dentry_routed(v_idx, local_parent, name).await? {
                Some((child_ino, _ft)) => child_ino,
                None => {
                    return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("Dentry {} not found in parent {}", name, parent),
                    )))
                }
            }
        };
        self.getattr(child_ino).await
    }

    async fn create_with_rdev(
        &self,
        parent: Ino,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
    ) -> Result<Inode> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[parent]) {
            return r.create_with_rdev(parent, name, mode, uid, gid, rdev).await;
        }
        self.create_with_rdev_size(parent, name, mode, uid, gid, rdev, 0)
            .await
    }

    async fn unlink(&self, parent: Ino, name: &str) -> Result<Ino> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[parent]) {
            return r.unlink(parent, name).await;
        }
        // PR 7b (design §5.6.5): the name of a STRIPED parent is removed
        // from its stripe (re-homed there first if the directory's own
        // tree still held it); a striped CHILD directory is taken down by
        // the rmdir protocol — ONE intent under ONE guard set.
        let (key_parent, striped_child) = self.stripe_unlink_prelude(parent, name).await?;
        match striped_child {
            Some(map) => self.rmdir_striped(key_parent, name, &map).await,
            None => self.unlink_at(key_parent, name).await,
        }
    }
    async fn link(&self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Inode> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[ino, new_parent]) {
            return r.link(ino, new_parent, new_name).await;
        }
        // PR 7b: a new name in a STRIPED directory is keyed under its
        // stripe (the EEXIST screen covers both homes).
        let dest_route = self.stripe_route(new_parent, new_name).await?;
        let new_parent = match &dest_route {
            Some(route) => {
                self.stripe_insert_parent(new_parent, new_name, route)
                    .await?
            }
            None => new_parent,
        };
        // S10 coherence law (rung 12): the parent's dentry set and the
        // linked inode's nlink/ctime both mutate — both participants are
        // parameters, so no resolution is needed.
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[new_parent, ino]).await;
        // §5.5.2a cutover gate — both inos are parameters, both slots
        // declared before any 4a acquisition (and before route
        // derivation: a park can span a flip).
        let _gate = self.slot_gate_enter(&[ino, new_parent]).await;
        // §5.4a M1: `link`'s participants ARE named, so `route_verb` and
        // the owner side already refuse a shipped one — this is the same
        // refusal on the LOCAL path, where no router runs.
        self.refuse_cross_owner_participants(crate::meta_ship::MetaVerb::Link, &[ino, new_parent])?;
        let (parent_v_idx, local_parent) = self.route_ino(new_parent);
        let (child_v_idx, local_child) = self.route_ino(ino);
        self.check_volume_enabled(parent_v_idx)?;
        self.check_volume_enabled(child_v_idx)?;

        // Both inodes are parameters: acquire the full set upfront —
        // per-volume sets in ascending volume order, each internally
        // canonical (taking I{child} after the D-guard would be an ABBA
        // inversion under stripe collisions).
        let mut _guards = Vec::new();
        let mut scope = None;
        if child_v_idx == parent_v_idx {
            _guards.extend(
                self.lock_many_leased(
                    parent_v_idx,
                    &mut scope,
                    &[
                        (local_parent, dlm::LockMode::Exclusive),
                        (local_child, dlm::LockMode::Exclusive),
                    ],
                    &[(local_parent, new_name, dlm::LockMode::Exclusive)],
                )
                .await?,
            );
        } else if child_v_idx < parent_v_idx {
            _guards.extend(
                self.lock_many_leased(
                    child_v_idx,
                    &mut scope,
                    &[(local_child, dlm::LockMode::Exclusive)],
                    &[],
                )
                .await?,
            );
            _guards.extend(
                self.lock_many_leased(
                    parent_v_idx,
                    &mut scope,
                    &[(local_parent, dlm::LockMode::Exclusive)],
                    &[(local_parent, new_name, dlm::LockMode::Exclusive)],
                )
                .await?,
            );
        } else {
            _guards.extend(
                self.lock_many_leased(
                    parent_v_idx,
                    &mut scope,
                    &[(local_parent, dlm::LockMode::Exclusive)],
                    &[(local_parent, new_name, dlm::LockMode::Exclusive)],
                )
                .await?,
            );
            _guards.extend(
                self.lock_many_leased(
                    child_v_idx,
                    &mut scope,
                    &[(local_child, dlm::LockMode::Exclusive)],
                    &[],
                )
                .await?,
            );
        }

        // PR M7 (Issue 13): Arc the op's guard set for its commit(s).
        let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(_guards);

        // PR 7b: a stripe an `rmdir`'s intent marked dying refuses the
        // insert under the guard (R26's closer — every insert path).
        if dest_route.is_some() {
            self.refuse_dying_parent(new_parent).await?;
        }
        if self
            .find_dentry_routed(parent_v_idx, local_parent, new_name)
            .await?
            .is_some()
        {
            return Err(crate::error::SqueezefsError::already_exists(
                "File already exists",
            ));
        }

        // Symmetric PR 6: a parent or inode in another appender's slot
        // takes the transaction path below — its steps route by slot.
        if parent_v_idx == child_v_idx && !self.spans_foreign_slot(&[ino, new_parent]) {
            // Same-volume link: ONE whole-tx entry (nlink+1 + dentry +
            // parent times).
            let be = &self.volumes[parent_v_idx];
            let out = be
                .routed_link_local(local_parent, new_name, local_child, ino, guards)
                .await;
            if out.is_err() {
                self.mirror_volume_failure(parent_v_idx);
            }
            out
        } else {
            // **Cross-volume link is ONE cross-volume transaction**
            // (DUR-7, design-cow-kv-metadata §4.10a). The count half stays
            // FIRST — it is the half that can refuse (EMLINK), and its
            // crash residue (`nlink` up, no second name) is a leak rather
            // than a dentry naming an under-counted inode — and it now
            // carries the intent record, so the next mount rolls the
            // second name forward instead of leaking the inode and every
            // block it names forever.
            let pre = self.volumes[child_v_idx]
                .read_inode_value_routed(local_child)
                .await?
                .ok_or_else(|| {
                    crate::error::SqueezefsError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("Inode {local_child} not found"),
                    ))
                })?;
            if pre.nlink >= 65000 {
                return Err(crate::error::SqueezefsError::too_many_links(
                    "Too many links",
                ));
            }
            let plan = crossvol_tx::XvPlan {
                op: crossvol_tx::XvOp::Link,
                steps: vec![
                    crossvol_tx::XvStep::SetNlink {
                        ino,
                        pre: pre.nlink,
                        post: pre.nlink + 1,
                        ctime: Some(kv::backend::KvMetaBackend::now_ns_pub()),
                    },
                    crossvol_tx::XvStep::InsertDentry {
                        parent: new_parent,
                        name: new_name.to_string(),
                        child: ino,
                        ft_bits: pre.mode & libc::S_IFMT,
                        parent_update: crossvol_tx::parent_update_code(
                            kv::backend::RoutedParentUpdate::ExclusiveTimes,
                        ),
                    },
                ],
            };
            let done = match crossvol_tx::execute(self, &plan, guards).await {
                Ok(d) => d,
                // PR 7b: the holder's witness refusal of an insert into a
                // dying stripe is the plan's `EEXIST`; the op's is `ENOENT`.
                Err(e) if dest_route.is_some() => {
                    return Err(self.dying_parent_errno(new_parent, e).await)
                }
                Err(e) => return Err(e),
            };
            // The reply is served from the count step's post-image (the
            // applier folds the pending-times refinement into it — the
            // generic/423 monotone-ctime discipline).
            let v = done.inode(0).cloned().unwrap_or(pre);
            Ok(Inode {
                ino,
                mode: v.mode,
                uid: v.uid,
                gid: v.gid,
                size: v.size,
                nlink: v.nlink,
                atime: v.atime,
                mtime: v.mtime,
                ctime: v.ctime,
                flags: v.flags,
                rdev: v.rdev,
            })
        }
    }

    async fn rename(
        &self,
        old_parent: Ino,
        old_name: &str,
        new_parent: Ino,
        new_name: &str,
        flags: u32,
    ) -> Result<()> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[old_parent, new_parent]) {
            return r
                .rename(old_parent, old_name, new_parent, new_name, flags)
                .await;
        }
        if flags & (libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE)
            == (libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE)
        {
            return Err(crate::error::SqueezefsError::Io(
                std::io::Error::from_raw_os_error(libc::EINVAL),
            ));
        }
        // The VFS forbids WHITEOUT|EXCHANGE; a defensive refusal keeps
        // the combination unrepresentable below (generic/631 family).
        if flags & libc::RENAME_WHITEOUT != 0 && flags & libc::RENAME_EXCHANGE != 0 {
            return Err(crate::error::SqueezefsError::Io(
                std::io::Error::from_raw_os_error(libc::EINVAL),
            ));
        }
        // PR 7b (design §5.6.5): each side of a rename under a STRIPED
        // parent is keyed under that name's STRIPE — a name still in the
        // directory's own tree (the source, or an overwrite/exchange
        // destination) is re-homed there first (`stripe_mutation_parent`,
        // Issue 4), so the body's revalidation under its guards is exact
        // against one home. A rename within one striped directory across
        // two stripes is then the ordinary two-parent rename, cross-owner
        // when the stripes' holders differ (PR 6's intent). The ancestry
        // walk stays exact through a stripe: a stripe is a child of its
        // directory in the dentry graph (the map entry names it), so a hop
        // that lands on a stripe continues to the directory.
        let old_parent = match self.stripe_route(old_parent, old_name).await? {
            Some(route) => {
                self.stripe_mutation_parent(old_parent, old_name, &route)
                    .await?
            }
            None => old_parent,
        };
        let dest_route = self.stripe_route(new_parent, new_name).await?;
        let new_parent = match &dest_route {
            Some(route) => {
                self.stripe_mutation_parent(new_parent, new_name, route)
                    .await?
            }
            None => new_parent,
        };
        let dest_is_stripe = dest_route.is_some();
        // Symmetric PR 6 (§5.6.4): the set-wide directory-rename lease is
        // the OUTERMOST lock — decided on an unguarded read of the
        // source's type (the body re-decides under its guards and hands
        // back `false` when the source became a directory meanwhile),
        // held for the op, released on every exit. Unarmed: one bool.
        let (old_parent_v_idx, _) = self.route_ino(old_parent);
        let armed = self
            .volumes
            .get(old_parent_v_idx)
            .is_some_and(|v| v.slot_lease_armed());
        let mut lease: Option<kv::backend::DirRenameLease> = None;
        let mut want_lock = armed
            && matches!(
                self.lookup_dentry_exact_unguarded(old_parent, old_name)
                    .await,
                Ok(Some((_, ft))) if ft == libc::S_IFDIR
            );
        loop {
            if want_lock && lease.is_none() {
                let t = std::time::Instant::now();
                let vol0 = &self.volumes[0];
                let identity = match crossvol_tx::TEST_DIR_RENAME_IDENTITY_ONCE
                    .swap(0, std::sync::atomic::Ordering::SeqCst)
                {
                    0 => vol0.own_appender_id(),
                    other => other,
                };
                let held = vol0
                    .dir_rename_lock_held(identity)
                    .await
                    .map_err(crate::error::SqueezefsError::from)?;
                crossvol_tx::note_dir_rename_lock(t.elapsed());
                lease = Some(held);
            }
            let out = self
                .rename_body(
                    old_parent,
                    old_name,
                    new_parent,
                    new_name,
                    flags,
                    lease.is_some(),
                    dest_is_stripe,
                )
                .await;
            if matches!(out, Ok(false)) {
                want_lock = true;
                continue;
            }
            if let Some(l) = lease.take() {
                l.release()
                    .await
                    .map_err(crate::error::SqueezefsError::from)?;
            }
            return match out {
                Ok(_) => Ok(()),
                // PR 7b: a shipped insert's witness refusal at a dying
                // stripe is the plan's `EEXIST`; the op's is `ENOENT`.
                Err(e) if dest_is_stripe => Err(self.dying_parent_errno(new_parent, e).await),
                Err(e) => Err(e),
            };
        }
    }

    async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[dir]) {
            return r.readdir(dir, offset, max).await;
        }
        // Rung 13 — the OQ-2 read gate's LOCAL face (see the trait
        // lookup's note).
        crate::meta_ship::deleg_read_gate(self, dir).await;
        self.readdir_local(dir, offset, max).await
    }

    // Takes a SHARED 4a lease internally — see the trait-level doc note
    // (VL8 item 6): exclusive-lease holders on the same stripe self-deadlock.
    async fn getattr(&self, ino: Ino) -> Result<Inode> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[ino]) {
            return r.getattr(ino).await;
        }
        self.getattr_local(ino).await
    }

    async fn setattr(
        &self,
        ino: Ino,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<u64>,
        mtime: Option<u64>,
        ctime: Option<u64>,
    ) -> Result<Inode> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[ino]) {
            return r
                .setattr(ino, mode, uid, gid, size, atime, mtime, ctime)
                .await;
        }
        // S10 coherence law (rung 12): attrs are exactly what a LOOKUP
        // delegation serves.
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[ino]).await;
        // §5.5.2a cutover gate — before the 4a I-guard (and before
        // route derivation: a park can span a flip).
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(vec![
            self.volumes[v_idx]
                .dlm()
                .lock_inode_exclusive(local_ino)
                .await,
        ]);
        let out = self.volumes[v_idx]
            .setattr_locked(local_ino, mode, uid, gid, size, atime, mtime, ctime, guards)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out.map(|mut i| {
            i.ino = ino;
            i
        })
    }

    async fn getxattr(&self, ino: Ino, name: &str) -> Result<Option<Vec<u8>>> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[ino]) {
            return r.getxattr(ino, name).await;
        }
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_ino).await;
        self.volumes[v_idx].getxattr(local_ino, name).await
    }

    async fn setxattr(&self, ino: Ino, name: &str, value: &[u8]) -> Result<()> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[ino]) {
            return r.setxattr(ino, name, value).await;
        }
        // S10 coherence law (rung 12): an xattr change moves ctime — the
        // delegated getattr's truth.
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[ino]).await;
        // §5.5.2a cutover gate — before the 4a I-guard (and before
        // route derivation: a park can span a flip).
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(vec![
            self.volumes[v_idx]
                .dlm()
                .lock_inode_exclusive(local_ino)
                .await,
        ]);
        let out = self.volumes[v_idx]
            .setxattr_locked(local_ino, name, value, guards)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    async fn removexattr(&self, ino: Ino, name: &str) -> Result<()> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[ino]) {
            return r.removexattr(ino, name).await;
        }
        // S10 coherence law (rung 12): ctime moves (the setxattr twin).
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[ino]).await;
        // §5.5.2a cutover gate — before the 4a I-guard (and before
        // route derivation: a park can span a flip).
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(vec![
            self.volumes[v_idx]
                .dlm()
                .lock_inode_exclusive(local_ino)
                .await,
        ]);
        let out = self.volumes[v_idx]
            .removexattr_locked(local_ino, name, guards)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    async fn listxattr(&self, ino: Ino) -> Result<Vec<String>> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[ino]) {
            return r.listxattr(ino).await;
        }
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let _guard = self.volumes[v_idx].dlm().lock_inode_shared(local_ino).await;
        self.volumes[v_idx].listxattr(local_ino).await
    }

    async fn destroy_inode(&self, ino: Ino) -> Result<()> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[ino]) {
            return r.destroy_inode(ino).await;
        }
        // S10 coherence law (rung 12): a destroyed record must not stay
        // servable under a grant (the unlink already recalled the parent;
        // this covers the object itself).
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[ino]).await;
        // §5.5.2a cutover gate — before the backend's own locks and
        // before route derivation.
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        Metadata::destroy_inode(self.volumes[v_idx].as_ref(), local_ino).await
    }
}

impl RoutedMetaBackend {
    /// [`Metadata::unlink`]'s body with the KEY parent resolved (PR 7b
    /// routes a striped parent's name to its stripe before this runs).
    async fn unlink_at(&self, parent: Ino, name: &str) -> Result<Ino> {
        // S10 coherence law (rung 12): the parent's dentry set mutates,
        // and the CHILD may itself be a delegated object (rmdir of a
        // delegated directory). The child read is paid only while
        // delegations are actually outstanding on this host; the
        // resolution is advisory exactly like the owner's cross-owner
        // pre-check — the recall correctness backstop is the grant
        // decline the held permit enforces.
        let mut deleg_set = vec![parent];
        if crate::meta_ship::deleg_gate_wants_children(self) {
            if let Ok(Some((child, _))) = self.lookup_dentry(parent, name).await {
                deleg_set.push(child);
            }
        }
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &deleg_set).await;
        // §5.5.2a cutover gate — before any 4a acquisition (and before
        // route derivation: a park can span a flip); the child's slot
        // joins after phase-1 discovery (holding nothing).
        let mut _gate = self.slot_gate_enter(&[parent]).await;

        // Two-phase child discovery (see dlm.rs): the child's I-lock may
        // live on any volume and must never be taken while holding this
        // volume's D-lock. Phase 1 reads the child; phase 2 re-locks the
        // full set — per-volume sets in ascending volume order, each set
        // internally canonical — and revalidates the dentry.
        //
        // Parent lock mode mirrors create (design §3.8): regular unlink
        // touches only the parent's mtime/ctime — SHARED parent, so
        // same-directory delete storms overlap. Directory removal mutates
        // parent nlink — EXCLUSIVE.
        let (
            file_type,
            global_child_ino,
            child_v_idx,
            local_child,
            parent_shared,
            guards,
            parent_v_idx,
            local_parent,
        ) = loop {
            // Fresh routes per iteration (the parked-extend `continue`
            // path can cross a flip).
            let (parent_v_idx, local_parent) = self.route_ino(parent);
            self.check_volume_enabled(parent_v_idx)?;
            let phase1 = self
                .lock_many_leased_discovery(
                    parent_v_idx,
                    &[(local_parent, dlm::LockMode::Shared)],
                    &[(local_parent, name, dlm::LockMode::Exclusive)],
                )
                .await?;
            let Some((global_child_ino, file_type)) = self
                .find_dentry_routed(parent_v_idx, local_parent, name)
                .await?
            else {
                return Err(crate::error::SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Dentry not found",
                )));
            };
            let (child_v_idx, local_child) = self.route_ino(global_child_ino);
            self.check_volume_enabled(child_v_idx)?;

            // Self-references and directories take the exclusive-parent
            // path; a regular file can never be its own parent.
            let parent_shared = file_type != libc::S_IFDIR && parent != global_child_ino;
            let parent_mode = if parent_shared {
                dlm::LockMode::Shared
            } else {
                dlm::LockMode::Exclusive
            };
            drop(phase1);
            // Gate the discovered child's slot BEFORE the phase-2 lock
            // set (we hold nothing here — a park is legal; re-discovery
            // iterations dedupe through the pass). A park can span a
            // flip: rediscover with FRESH routes.
            if self.slot_gate_extend(&mut _gate, &[global_child_ino]).await {
                continue;
            }

            let mut guards = Vec::new();
            let mut scope = None;
            if parent == global_child_ino {
                guards.extend(
                    self.lock_many_leased(
                        parent_v_idx,
                        &mut scope,
                        &[(local_parent, dlm::LockMode::Exclusive)],
                        &[(local_parent, name, dlm::LockMode::Exclusive)],
                    )
                    .await?,
                );
            } else if child_v_idx == parent_v_idx {
                guards.extend(
                    self.lock_many_leased(
                        parent_v_idx,
                        &mut scope,
                        &[
                            (local_parent, parent_mode),
                            (local_child, dlm::LockMode::Exclusive),
                        ],
                        &[(local_parent, name, dlm::LockMode::Exclusive)],
                    )
                    .await?,
                );
            } else if child_v_idx < parent_v_idx {
                guards.extend(
                    self.lock_many_leased(
                        child_v_idx,
                        &mut scope,
                        &[(local_child, dlm::LockMode::Exclusive)],
                        &[],
                    )
                    .await?,
                );
                guards.extend(
                    self.lock_many_leased(
                        parent_v_idx,
                        &mut scope,
                        &[(local_parent, parent_mode)],
                        &[(local_parent, name, dlm::LockMode::Exclusive)],
                    )
                    .await?,
                );
            } else {
                guards.extend(
                    self.lock_many_leased(
                        parent_v_idx,
                        &mut scope,
                        &[(local_parent, parent_mode)],
                        &[(local_parent, name, dlm::LockMode::Exclusive)],
                    )
                    .await?,
                );
                guards.extend(
                    self.lock_many_leased(
                        child_v_idx,
                        &mut scope,
                        &[(local_child, dlm::LockMode::Exclusive)],
                        &[],
                    )
                    .await?,
                );
            }

            match self
                .find_dentry_routed(parent_v_idx, local_parent, name)
                .await?
            {
                Some((cur_child, _)) if cur_child == global_child_ino => {
                    // PR M7 (Issue 13): Arc the op's guard set — each
                    // commit below co-owns it.
                    let guards: std::sync::Arc<[dlm::DlmGuard]> = std::sync::Arc::from(guards);
                    break (
                        file_type,
                        global_child_ino,
                        child_v_idx,
                        local_child,
                        parent_shared,
                        guards,
                        parent_v_idx,
                        local_parent,
                    );
                }
                _ => continue, // dentry changed under us — rediscover
            }
        };

        // **§5.4a M1** — the DISCOVERED participant the router could not
        // see (`named_inos` answers the parent alone for `Unlink`), pinned
        // here rather than at the plan: refuse before any durable effect.
        self.refuse_cross_owner_participants(
            crate::meta_ship::MetaVerb::Unlink,
            &[global_child_ino],
        )?;

        let is_dir = file_type == libc::S_IFDIR;
        // Symmetric PR 6: a parent or child in another appender's slot
        // takes the transaction path below — its steps route by slot.
        if parent_v_idx == child_v_idx && !self.spans_foreign_slot(&[parent, global_child_ino]) {
            // Same-volume unlink: ONE whole-tx entry with the routed
            // semantics.
            let be = &self.volumes[parent_v_idx];
            let out = be
                .routed_unlink_local(
                    local_parent,
                    name,
                    local_child,
                    is_dir,
                    parent_shared,
                    guards,
                )
                .await;
            if out.is_err() {
                self.mirror_volume_failure(parent_v_idx);
            }
            out?;
            Ok(global_child_ino)
        } else {
            // **Cross-volume unlink is ONE cross-volume transaction**
            // (DUR-7, design-cow-kv-metadata §4.10a). The effect order is
            // unchanged — the name goes first, because that is the half
            // the caller is told about — but the parent-side commit now
            // carries the intent record, so a crash between the halves is
            // rolled forward at the next mount instead of leaving
            // `nlink = 1` with zero dentries: invisible, and skipped
            // forever by `reclaim_orphaned_batch`'s `nlink > 0` rule.
            let update = if parent_shared {
                kv::backend::RoutedParentUpdate::SharedTimes
            } else if is_dir {
                kv::backend::RoutedParentUpdate::ExclusiveTimesBump
            } else {
                kv::backend::RoutedParentUpdate::ExclusiveTimes
            };
            // The name always goes; the count step exists only if there is
            // an inode record to count. A dentry naming a destroyed inode
            // must stay REMOVABLE (the pre-S3.5 fragment order removed the
            // name and then errored, which at least made `rm` work; a plan
            // that refused up front would strand the name forever).
            let mut steps = vec![crossvol_tx::XvStep::RemoveDentry {
                parent,
                name: name.to_string(),
                expect_child: global_child_ino,
                parent_update: crossvol_tx::parent_update_code(update),
            }];
            match self.volumes[child_v_idx]
                .read_inode_value_routed(local_child)
                .await?
            {
                // Validation + the count step's (pre, post) witness, read
                // under the guards this op already holds.
                Some(child_v) => steps.push(crossvol_tx::XvStep::SetNlink {
                    ino: global_child_ino,
                    pre: child_v.nlink,
                    post: if is_dir {
                        0
                    } else {
                        child_v.nlink.saturating_sub(1)
                    },
                    ctime: Some(kv::backend::KvMetaBackend::now_ns_pub()),
                }),
                None => log::warn!(
                    "cross-volume unlink of {name:?} in parent {parent}: child ino \
                     {global_child_ino} has no inode record — removing the dangling name \
                     and accounting nothing (run `squeezefs fsck`)"
                ),
            }
            let plan = crossvol_tx::XvPlan {
                op: if is_dir {
                    crossvol_tx::XvOp::Rmdir
                } else {
                    crossvol_tx::XvOp::Unlink
                },
                steps,
            };
            crossvol_tx::execute(self, &plan, guards).await?;
            Ok(global_child_ino)
        }
    }
}

impl RoutedMetaBackend {
    pub async fn sync_all_devices(&self) -> Result<()> {
        for vol in &self.volumes {
            // PR M6: parked times refinements ride this durability point
            // (fsyncdir/syncfs-class callers) — journal them BEFORE the
            // barrier so the barrier covers them.
            vol.drain_pending_times_now().await?;
            // The coalesced barrier ALSO drains the §4.6 pt 3
            // pending-reclaim bookkeeping.
            vol.sync_device().await?;
        }
        Ok(())
    }

    /// fdatasync only the MetaLV volume that owns `ino` (avoid multi-volume fsync tax).
    pub async fn sync_device_for_ino(&self, ino: Ino) -> Result<()> {
        let (v_idx, _) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        crate::fuse_client::METRICS
            .meta_sync_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // PR M6: fsync(ino) durability covers the ino's absorbed times
        // refinement — drain the volume's parked set (batched, usually
        // empty) ahead of the barrier.
        self.volumes[v_idx].drain_pending_times_now().await?;
        self.volumes[v_idx].sync_device().await
    }

    /// Persist layout xattr + size with fine locks. Used on fsync/release
    /// writeback; ONE two-record transaction (design §5.3, its own
    /// I-guard).
    pub async fn set_layout_and_size(
        &self,
        ino: Ino,
        layout: &[u8],
        size: u64,
        block_refs: &[crate::meta_backend::kv::block_refs::BlockRefOp],
    ) -> Result<()> {
        // S10 coherence law (rung 12): a layout publish changes the
        // object's size/mtime — the CONFLICTING PUBLISH the design's
        // recall-before-conflicting-publish clause names. Covers the
        // authority's own writeback AND the S9 shipped publishes (both
        // funnel here).
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[ino]).await;
        // §5.5.2a cutover gate — before the backend's own I-guard and
        // before route derivation.
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let block_refs = self.forest_ref_ops(v_idx, block_refs);
        let out = self.volumes[v_idx]
            .set_layout_and_size(local_ino, layout, size, &block_refs)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    /// **[`Self::set_layout_and_size`] over a SET of distinct inos, committed
    /// as one conveyor group per volume** (D-1c — e2e perf audit §5.3 row 1,
    /// one conveyor group per shipped frame). Per item the semantics are the
    /// single verb's exactly: the S10 coherence gate and the §5.5.2a cutover
    /// gate are held until the item's terminal outcome, `route_ino` after the
    /// gates, `check_volume_enabled`, the volume's fail-stop mirrored on error.
    /// What differs is the commit: the items are grouped by home volume, each
    /// volume's members take ONE canonical `lock_many` over their inos
    /// (`dlm.rs`'s acquisition law — distinct inos may share a stripe), stage
    /// concurrently under that shared guard set, and commit through
    /// [`kv::backend::KvMetaBackend::commit_tx_group`] — one queue-lock
    /// enqueue, so the set is one apply pass by construction. One tx = one
    /// checksummed journal entry is unchanged.
    ///
    /// Outcomes are returned in input order. An item that fails BEFORE the
    /// group (a disabled volume, a stage error such as a missing inode) gets
    /// its own `Err` and never joins the group; its siblings are unaffected.
    /// Inos must be distinct within one call — the publish owner's chain
    /// partition guarantees it (one call per chain per round), so a duplicate
    /// is refused loud (its later occurrence) rather than serialized: two
    /// staged txs for one ino in one group would race on the RAM view.
    pub async fn set_layout_and_size_group(&self, items: Vec<LayoutPublish>) -> Vec<Result<()>> {
        match self.set_layout_and_size_group_inner(items, false).await {
            LayoutGroupCommit::Committed(results) => results,
            // Unreachable: the single-volume law is only asked for.
            LayoutGroupCommit::Split { volumes } => {
                vec![Err(crate::error::SqueezefsError::InvalidOperation(
                    format!(
                        "layout publish group reported a {volumes}-volume split without the \
                     single-volume law (unreachable)"
                    ),
                ))]
            }
        }
    }

    /// **[`Self::set_layout_and_size_group`] under the PACK-GROUP law**
    /// (design-small-file-packing §5.6, PK4): the items must route to ONE
    /// home meta volume, judged AFTER the §5.5.2a slot gate — the gate's own
    /// law ("parks can span a flip, so callers must re-derive routes taken
    /// before the call") is exactly the window an online `migrate-meta-slot`
    /// cutover can land in between a co-writer's partition and this serve.
    /// A frame whose members bucket into two volumes would be two
    /// independently committed sub-groups, i.e. the publish-after-terminal-
    /// free window the law closes — so it commits NOTHING and answers
    /// [`LayoutGroupCommit::Split`] (the owner refuses the frame
    /// `PUBLISH_PACK_GROUP_SPLIT`). One check, after routing, under the gate.
    pub async fn set_layout_and_size_pack_group(
        &self,
        items: Vec<LayoutPublish>,
    ) -> LayoutGroupCommit {
        self.set_layout_and_size_group_inner(items, true).await
    }

    async fn set_layout_and_size_group_inner(
        &self,
        items: Vec<LayoutPublish>,
        single_home_volume: bool,
    ) -> LayoutGroupCommit {
        let n = items.len();
        let mut results: Vec<Option<Result<()>>> = (0..n).map(|_| None).collect();
        if n == 0 {
            return LayoutGroupCommit::Committed(Vec::new());
        }
        let inos: Vec<Ino> = items.iter().map(|it| it.ino).collect();
        // S10 coherence law (rung 12) — the single verb's gate over the
        // whole set: recall every outstanding delegation on the named
        // objects, hold the permit across the group.
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &inos).await;
        // §5.5.2a cutover gate — before the backend's own I-guards and
        // before route derivation (a park can span a flip).
        let _gate = self.slot_gate_enter(&inos).await;

        // Route, screen, and bucket by home volume (input index kept).
        let mut seen: std::collections::HashSet<Ino> = std::collections::HashSet::with_capacity(n);
        let mut per_volume: std::collections::BTreeMap<usize, Vec<(usize, Ino, LayoutPublish)>> =
            std::collections::BTreeMap::new();
        for (i, item) in items.into_iter().enumerate() {
            if !seen.insert(item.ino) {
                crate::note_invariant_tripwire(
                    "set_layout_and_size_group_duplicate_ino",
                    &format!(
                        "ino {} named twice in one group — the caller's partition law (one \
                         call per named ino per group) is broken; refusing the duplicate \
                         rather than racing two staged txs on one ino",
                        item.ino
                    ),
                );
                results[i] = Some(Err(crate::error::SqueezefsError::InvalidOperation(
                    format!(
                        "layout publish group names ino {} twice (one call per ino per group)",
                        item.ino
                    ),
                )));
                continue;
            }
            let (v_idx, local_ino) = self.route_ino(item.ino);
            if let Err(e) = self.check_volume_enabled(v_idx) {
                results[i] = Some(Err(e));
                continue;
            }
            per_volume
                .entry(v_idx)
                .or_default()
                .push((i, local_ino, item));
        }
        // PK4: the post-gate single-volume check — the routes above are the
        // re-derived ones (taken after the gate admitted the set).
        if single_home_volume && per_volume.len() > 1 {
            return LayoutGroupCommit::Split {
                volumes: per_volume.len(),
            };
        }

        // Every volume's sub-group runs independently and concurrently: a
        // volume's members hold only THAT volume's guards (cross-volume
        // lock sets stay in ascending volume order by never being held
        // together here).
        let futs = per_volume.into_iter().map(|(v_idx, members)| async move {
            let vol = &self.volumes[v_idx];
            let want: Vec<(Ino, dlm::LockMode)> = members
                .iter()
                .map(|(_, local, _)| (*local, dlm::LockMode::Exclusive))
                .collect();
            // ONE canonical acquisition for the whole sub-group (deduped by
            // stripe, ascending), co-owned by every member tx until its
            // terminal outcome.
            let guards: std::sync::Arc<[dlm::DlmGuard]> =
                std::sync::Arc::from(vol.dlm().lock_many(&want, &[]).await);
            let staged = futures::future::join_all(members.iter().map(|(_, local, item)| {
                let guards = std::sync::Arc::clone(&guards);
                async move {
                    vol.stage_layout_and_size_holding(
                        *local,
                        &item.layout,
                        item.size,
                        &self.forest_ref_ops(v_idx, &item.block_refs),
                        guards,
                    )
                    .await
                }
            }))
            .await;
            drop(guards);
            let mut outcomes: Vec<(usize, Result<()>)> = Vec::with_capacity(members.len());
            let mut txs = Vec::with_capacity(members.len());
            let mut tx_slots = Vec::with_capacity(members.len());
            for ((i, _, _), staged) in members.iter().zip(staged) {
                match staged {
                    Ok(tx) => {
                        txs.push(tx);
                        tx_slots.push(*i);
                    }
                    Err(e) => outcomes.push((*i, Err(e))),
                }
            }
            if !txs.is_empty() {
                for (i, out) in tx_slots.into_iter().zip(vol.commit_tx_group(txs).await) {
                    outcomes.push((i, out.map_err(Into::into)));
                }
            }
            (v_idx, outcomes)
        });
        for (v_idx, outcomes) in futures::future::join_all(futs).await {
            for (i, out) in outcomes {
                if out.is_err() {
                    self.mirror_volume_failure(v_idx);
                }
                results[i] = Some(out);
            }
        }
        LayoutGroupCommit::Committed(
            results
                .into_iter()
                .map(|slot| {
                    slot.unwrap_or_else(|| {
                        Err(crate::error::SqueezefsError::InvalidOperation(
                            "layout publish group: a member reached no outcome (unreachable — \
                             every member is refused, staged-and-failed, or committed)"
                                .to_string(),
                        ))
                    })
                })
                .collect(),
        )
    }

    /// Spec §6.2 item 1: commit a standalone durable block-reference
    /// operation set for `ino` (the reclaim release / fsck repair seam) on
    /// the volume that hosts it.
    pub async fn commit_block_refs(
        &self,
        ino: Ino,
        ops: &[crate::meta_backend::kv::block_refs::BlockRefOp],
    ) -> Result<()> {
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let ops = self.forest_ref_ops(v_idx, ops);
        let out = self.volumes[v_idx].commit_block_refs(local_ino, &ops).await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    /// [`Self::commit_block_refs`] with the release WITNESS
    /// ([`kv::backend::KvMetaBackend::commit_block_refs_witnessed`]): `None`
    /// when `ino`'s home volume carries no ledger, `Some(held)` otherwise —
    /// the reclaim path's RAM-decrement gate.
    pub async fn commit_block_refs_witnessed(
        &self,
        ino: Ino,
        ops: &[crate::meta_backend::kv::block_refs::BlockRefOp],
    ) -> Result<Option<Vec<crate::meta_backend::kv::block_refs::BlockRef>>> {
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let ops = self.forest_ref_ops(v_idx, ops);
        let out = self.volumes[v_idx]
            .commit_block_refs_witnessed(local_ino, &ops)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    /// Symmetric PR 7 — `MarkShared { F, b… }` through the routed layer's
    /// DOOR (the shape of [`Self::commit_block_refs`], every 4a-guarded
    /// mutation of `ino`'s records takes): a peer-owned ino (S9's verb
    /// router names an owner) refuses loud — the wire `MarkShared` at a
    /// foreign holder is PR 12's; the S10 delegation gate and the §5.5.2a
    /// cutover gate (a slot mid-`migrate-meta-slot` parks the mark before
    /// 4a exactly as it parks a layout commit), the volume's enabled
    /// check, then the volume's one-tx executor on `refs` — which the
    /// caller already keys in the VOLUME's key form (the local key ino on
    /// a forest — the clone path builds them beside the index's global
    /// entries).
    pub async fn mark_block_refs_shared(
        &self,
        ino: Ino,
        refs: &[crate::meta_backend::kv::block_refs::BlockRef],
    ) -> Result<Vec<crate::meta_backend::kv::shared_refs::MarkOutcome>> {
        if let Some(r) = crate::meta_ship::daemon_verb_router(self, &[ino]) {
            let owner = r
                .owner_for_ino(ino)
                .map(|o| o.endpoint.clone())
                .unwrap_or_else(|| "a peer".to_string());
            return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                "MarkShared: ino {ino} is owned by {owner} — the wire MarkShared at a foreign \
                 slot holder lands with the symmetric program's PR 12"
            )));
        }
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[ino]).await;
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, _local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let out = self.volumes[v_idx].mark_block_refs_shared(refs).await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    /// PR 2 (kvmap): is the block-map tree engaged on `ino`'s HOME volume
    /// (incompat bit 16 stamped, tree mounted)? The crossing decision's
    /// probe — `false` keeps the legacy indirect-blob arm.
    pub fn block_map_tree_engaged(&self, ino: Ino) -> bool {
        let (v_idx, _) = self.route_ino(ino);
        self.volumes[v_idx].block_map_tree_engaged()
    }

    /// Finding 43 — the SELF-ARMING probe for a local NEW crossing
    /// (design §2, the bit-5 law: the ratchet engages at first use,
    /// never on untouched volumes): routes to `ino`'s home volume and
    /// runs its bit-16 ratchet + tree mint. `false` = the ratchet could
    /// not complete — the caller keeps the legacy blob arm, never blocks
    /// the write.
    pub async fn block_map_tree_ready(&self, ino: Ino) -> bool {
        let (v_idx, _) = self.route_ino(ino);
        if self.check_volume_enabled(v_idx).is_err() {
            return false;
        }
        self.volumes[v_idx].block_map_tree_ready().await
    }

    /// PR 2 (kvmap): a bounded window of `ino`'s tree-7 mappings from
    /// `from_index` upward, routed to its home volume (records key on the
    /// volume-LOCAL ino — the `inode_key` identity the per-volume walkers
    /// join against layout heads).
    ///
    /// PR 6a (design §12): a `from_index` landing strictly INSIDE a
    /// preceding run's span PREPENDS that covering run — record-true
    /// (the run rides verbatim, keyed at its true start; the consumer
    /// clips/overlays), so a fresh mid-run window never reads a covered
    /// index as absent. Continuation loops (cursor = last key + 1) get
    /// the same record re-delivered when their cursor lands mid-span —
    /// their per-index overlay is idempotent, and a page whose every
    /// record precedes the cursor means the walk is DONE (the
    /// no-progress guard; the per-volume primitive stays strictly
    /// record-true for the sweep/C11 count loops).
    pub async fn block_map_range(
        &self,
        ino: Ino,
        from_index: u32,
        max: usize,
    ) -> Result<Vec<(u32, crate::meta_backend::kv::block_map::MapEntry)>> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let mut page = self.volumes[v_idx]
            .block_map_range(local_ino, from_index, max)
            .await
            .map_err(|e| {
                crate::error::SqueezefsError::InvalidOperation(format!(
                    "block-map range for ino {ino} failed: {e}"
                ))
            })?;
        if from_index > 0 && page.first().map(|(i, _)| *i) != Some(from_index) {
            if let Some((start, entry)) = self.volumes[v_idx]
                .block_map_floor(local_ino, from_index)
                .await
                .map_err(|e| {
                    crate::error::SqueezefsError::InvalidOperation(format!(
                        "block-map floor probe for ino {ino} failed: {e}"
                    ))
                })?
            {
                page.insert(0, (start, entry));
            }
        }
        Ok(page)
    }

    /// PR 2 (kvmap): the crossing/migration train, routed to `ino`'s home
    /// volume (see [`kv::backend::KvMetaBackend::migrate_block_map_train`]).
    /// The publish class of [`Self::set_layout_and_size`] — same
    /// delegation + cutover gates. `claims` selects PR 5b's claims-scoped
    /// mode (design §11 law b — every SHIPPED sticky-head train);
    /// `None` is the local whole-map diff (Rev 1.3 #2).
    pub async fn migrate_block_map_train(
        &self,
        ino: Ino,
        layout: &[u8],
        size: u64,
        block_refs: &[crate::meta_backend::kv::block_refs::BlockRefOp],
        entries: &[(u32, String)],
        chunk: usize,
        claims: Option<&crate::meta_backend::kv::backend::MapTrainClaims>,
        cursor_floor: u32,
        ref_for: &(dyn Fn(&str, u32) -> Option<crate::meta_backend::kv::block_refs::BlockRef>
              + Send
              + Sync),
    ) -> Result<Option<crate::meta_backend::kv::backend::MapMigrateOutcome>> {
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[ino]).await;
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        // PR 3 (Rev 1.3 #3): the record economy — encode each key string
        // to its tree-7 record form HERE (the last point with router
        // access on both the local and the served arm; the wire stays
        // strings, so the verb schema is untouched).
        let mut records: Vec<(u32, kv::block_map::MapEntry)> = entries
            .iter()
            .map(|(b, k)| (*b, self.encode_map_entry(k)))
            .collect();
        // PR 6a (design §12): the post-encode RUN seam — straight
        // sequential spans coalesce into RUN/RUN2 records on LOCAL
        // whole-map trains only. Claims-scoped (shipped/range-custody)
        // trains stay per-index BY LAW: adoption/release is claim-index
        // arithmetic, and a run a claim splits must never be
        // partial-adopted silently.
        if claims.is_none() {
            if let Some(stride_of) = self.map_run_stride.get() {
                records = coalesce_map_runs(records, stride_of.as_ref());
            }
        }
        // PR 5b: block refs key on the GLOBAL ino (the block_refs.rs key
        // law), so the train's recompute is handed both identities plus
        // the record decoder. PR 6a: the per-index delta form (run
        // records resolve covered indices through the stride census).
        let entry_key =
            |e: &kv::block_map::MapEntry, delta: u32| self.decode_map_entry_at(e, delta);
        let block_refs = self.forest_ref_ops(v_idx, block_refs);
        // The train's own reference builder keys the publishing ino: on a
        // forest volume that is its LOCAL key form (`forest_ref_ops`' law).
        let refs_owner = self.forest_refs_owner(v_idx, ino, local_ino);
        let ref_for_keyed = |key: &str, idx: u32| {
            ref_for(key, idx).map(|r| {
                if r.owner_ino == ino {
                    kv::shared_refs::with_owner(&r, refs_owner)
                } else {
                    r
                }
            })
        };
        let out = self.volumes[v_idx]
            .migrate_block_map_train(
                local_ino,
                layout,
                size,
                &block_refs,
                &records,
                chunk,
                claims,
                refs_owner,
                &entry_key,
                cursor_floor,
                &ref_for_keyed,
            )
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    /// PR 3 (kvmap, A9): the fetch bracket's epoch probe, routed to
    /// `ino`'s home volume — see
    /// [`kv::backend::KvMetaBackend::reader_fetch_epoch`]. `None` on
    /// write mounts (zero epoch reads, pinned) and un-armed readers.
    pub fn reader_fetch_epoch(&self, ino: Ino) -> Option<u64> {
        let (v_idx, _) = self.route_ino(ino);
        self.volumes.get(v_idx)?.reader_fetch_epoch()
    }

    /// PR 2 (kvmap): the unlink-side bounded record sweep, routed to
    /// `ino`'s home volume (never silent residue — Rev 1.1 #4).
    pub async fn sweep_block_map(&self, ino: Ino, chunk: usize) -> Result<u64> {
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let out = self.volumes[v_idx].sweep_block_map(local_ino, chunk).await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    /// PR 6b (design §3/A2): the size-flip-first truncate/unlink handoff
    /// — O(1), one tx: the new size + the durable per-ino sweep cursor in
    /// the head sentinel — routed to `ino`'s home volume with the
    /// `set_layout_and_size` gates (the publish class).
    pub async fn kvmap_truncate_handoff(
        &self,
        ino: Ino,
        new_size: u64,
        k: u32,
        block_refs: &[crate::meta_backend::kv::block_refs::BlockRefOp],
    ) -> Result<String> {
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[ino]).await;
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let block_refs = self.forest_ref_ops(v_idx, block_refs);
        let out = self.volumes[v_idx]
            .kvmap_truncate_handoff(local_ino, new_size, k, &block_refs)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    /// PR 6b: one A2 background-sweep chunk, routed to `ino`'s home
    /// volume — the per-chunk one-tx law (map Deletes + ref releases +
    /// cursor advance) with the freed keys returned for the caller's
    /// post-commit reclaim enqueue (RES-1). `entry_key`/`ref_for`
    /// resolution rides the same shared expansion surface the migrate
    /// train uses ([`Self::decode_map_entry_at`]).
    pub async fn kvmap_sweep_chunk(
        &self,
        ino: Ino,
        chunk: usize,
        floor_of: &(dyn Fn(u64) -> u32 + Send + Sync),
        ref_for: &(dyn Fn(&str, u32) -> Option<crate::meta_backend::kv::block_refs::BlockRef>
              + Send
              + Sync),
    ) -> Result<crate::meta_backend::kv::backend::SweepChunkOutcome> {
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[ino]).await;
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let entry_key =
            |e: &kv::block_map::MapEntry, delta: u32| self.decode_map_entry_at(e, delta);
        let out = self.volumes[v_idx]
            .kvmap_sweep_chunk(local_ino, chunk, floor_of, &entry_key, ref_for)
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    /// Write-commit-economy campaign: [`Self::set_layout_and_size`] with
    /// a layout **delta** carrying the publish batch — the volume stages
    /// an O(batch) delta record where a live inline base exists, the
    /// full layout otherwise. Returns whether the delta was staged.
    pub async fn merge_layout_and_size(
        &self,
        ino: Ino,
        delta: &crate::layout_wire::LayoutDelta,
        full_layout: bytes::Bytes,
        size: u64,
        block_refs: Vec<crate::meta_backend::kv::block_refs::BlockRefOp>,
    ) -> Result<bool> {
        // S10 coherence law (rung 12) — the `set_layout_and_size`
        // discipline: the delta publish is the same conflicting-publish
        // class.
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[ino]).await;
        // §5.5.2a cutover gate — before the backend's own I-guard and
        // before route derivation (the `set_layout_and_size` discipline).
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let block_refs = self.forest_ref_ops(v_idx, &block_refs).into_owned();
        let out = self.volumes[v_idx]
            .merge_layout_and_size(
                local_ino,
                self.forest_refs_owner(v_idx, ino, local_ino),
                delta,
                full_layout,
                size,
                block_refs,
            )
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    /// DLM S11 rung 17 — [`Self::merge_layout_and_size`] in **chain-onto-
    /// head** mode (KD-MW-8's composition law): the S9 publish serve and
    /// the authority's own granted-ino publishes route here, so a
    /// co-writer's delta composes onto whatever durable head its peers
    /// produced. Returns `(use_delta, staged_version)` — the staged
    /// link's version travels back on the publish reply (the
    /// design-mw-layout-versions §6 chain-without-refetch residual).
    pub async fn merge_layout_and_size_chained(
        &self,
        ino: Ino,
        delta: &crate::layout_wire::LayoutDelta,
        full_layout: bytes::Bytes,
        size: u64,
        block_refs: Vec<crate::meta_backend::kv::block_refs::BlockRefOp>,
    ) -> Result<(bool, u64)> {
        self.merge_layout_and_size_chained_accounted(ino, delta, full_layout, size, block_refs)
            .await
            .map(|(used, version, _recomputed)| (used, version))
    }

    /// [`Self::merge_layout_and_size_chained`] surfacing the finding-36
    /// owner-recompute verdict ([`crate::meta_backend::kv::backend::
    /// RecomputedReleases`]): the S9 publish serve runs the released set
    /// through the authority's own free ladder strictly after commit Ok
    /// and answers the reply's `recomputed` flag; every other caller uses
    /// the dropping wrapper above.
    pub async fn merge_layout_and_size_chained_accounted(
        &self,
        ino: Ino,
        delta: &crate::layout_wire::LayoutDelta,
        full_layout: bytes::Bytes,
        size: u64,
        block_refs: Vec<crate::meta_backend::kv::block_refs::BlockRefOp>,
    ) -> Result<crate::meta_backend::kv::backend::MergeOutcome> {
        let _deleg_gate = crate::meta_ship::deleg_mutation_gate(self, &[ino]).await;
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        let block_refs = self.forest_ref_ops(v_idx, &block_refs).into_owned();
        let out = self.volumes[v_idx]
            .merge_layout_and_size_chained_accounted(
                local_ino,
                self.forest_refs_owner(v_idx, ino, local_ino),
                delta,
                full_layout,
                size,
                block_refs,
            )
            .await;
        if out.is_err() {
            self.mirror_volume_failure(v_idx);
        }
        out
    }

    /// The ino's durable layout-chain HEAD version (0 = bare `Put` /
    /// no layout / unversioned head) — rung 17's covering-version probe
    /// (the `FlushExtents` reply and the retention release watermark
    /// read it).
    pub async fn layout_head_version(&self, ino: Ino) -> Result<u64> {
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        self.volumes[v_idx].layout_head_version(local_ino).await
    }

    /// Park a WRITE op's kernel-domain times stamp as the ino's pending
    /// refinement (generic/003 remount-divergence fix): the ONE durable
    /// authority for the write's mtime/ctime — the same stamp the FUSE
    /// handler publishes to the attr cache. RAM-parked (zero hot-path
    /// journal entries), fold-visible on every read, journaled by the
    /// M6 batched drain / the fsync/unmount durability points.
    pub async fn park_write_times(&self, ino: Ino, mtime: u64, ctime: u64) -> Result<()> {
        // §5.5.2a cutover gate — a mid-migration park must land on the
        // slot the drain will run against.
        let _gate = self.slot_gate_enter(&[ino]).await;
        let (v_idx, local_ino) = self.route_ino(ino);
        self.check_volume_enabled(v_idx)?;
        self.volumes[v_idx].park_times_refinement(local_ino, mtime, ctime);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PR VL5a: §5.5.1a mount-time bootstrap — stamp observation, canonical
// ordering, disagreement refusals, and the format-time slot plan.
// ---------------------------------------------------------------------------

/// One volume's raw §5.5.1a bootstrap observation: superblock uuid +
/// the newest ledger record's membership stamp (both read off the device
/// WITHOUT mounting — no tree routing, the chicken-and-egg resolution).
#[derive(Debug, Clone)]
pub struct MetaVolumeObservation {
    pub path: String,
    pub uuid: [u8; 16],
    pub stamp: Option<kv::checkpoint::MembershipStamp>,
}

/// Read every listed volume's §5.5.1a observation (sector 0 + the 128 KiB
/// ledger extent — cheap probe reads). Blank volumes and legacy v2 refuse
/// loud exactly like the mount gate; a volume whose ledger holds no valid
/// record observes as stampless (the mount proper will refuse it later —
/// this surface only classifies membership). Also the raw feed for
/// `squeezefs volume repair-set`'s observed-state report.
pub async fn observe_meta_set(paths: &[String]) -> Result<Vec<MetaVolumeObservation>> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        match kv::superblock::classify_volume(std::path::Path::new(path)).await? {
            kv::superblock::VolumeFormat::Blank => {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "Metadata volume {path} is not formatted (zeroed superblock) — run \
                     `squeezefs format` first"
                )));
            }
            kv::superblock::VolumeFormat::V2Legacy => {
                return Err(v2_unsupported_error(path));
            }
            kv::superblock::VolumeFormat::V3(sb) => {
                let stamp = kv::checkpoint::read_newest_ledger(
                    std::path::Path::new(path),
                    sb.root_ledger.start,
                )
                .await?
                .and_then(|rec| rec.membership_stamp);
                out.push(MetaVolumeObservation {
                    path: path.clone(),
                    uuid: sb.uuid,
                    stamp,
                });
            }
        }
    }
    Ok(out)
}

/// A discovered metadata set (§5.5.1a): the CANONICAL member order plus
/// the frozen routing geometry the mount routes with.
#[derive(Debug, Clone)]
pub struct MetaSetDiscovery {
    /// Member paths in canonical order — `member_position` for stamped
    /// sets, the operator's URI order (exactly today's behavior) for
    /// legacy sets.
    pub ordered_paths: Vec<String>,
    /// Superblock uuids in the same canonical order (the
    /// [`volume_set_generation`] inputs).
    pub uuids: Vec<[u8; 16]>,
    /// The stored (derived-at-format) routing width W.
    pub routing_width: u64,
    /// `slot → canonical volume index`.
    pub slot_to_volume: Vec<usize>,
    /// Per canonical volume, the slot whose records live in its LEGACY
    /// keyspace (`MembershipStamp::resolved_native_slot`).
    pub native_slots: Vec<Option<u16>>,
    /// PR VL5b: the highest membership epoch observed across the set
    /// (the flip protocol's clock; legacy sets report 0).
    pub set_epoch: u64,
}

/// §5.5.1a order-independent discovery: probe every URI-listed volume's
/// stamp FIRST (before any tree routing), reconstruct the membership by
/// `member_position`, cross-check `set_uuid`/`set_epoch`/completeness,
/// and only then let callers route ino 1. All-stampless sets refuse
/// loud (torn formats / foreign ledgers — every dynamic-routing format
/// stamps at format time; the legacy implicit-identity arm died with
/// the frozen widths, design-dynamic-meta-routing §5.6).
/// **Every disagreement is a loud refusal naming the volumes**: mixed
/// stamped/unstamped members, foreign `set_uuid`s, torn/mixed epochs
/// (crash mid-protocol — the refusal names `squeezefs volume
/// repair-set`), duplicate or out-of-range positions, member counts that
/// do not match the URI list, and slot maps that do not cover `[0, W)`
/// exactly once.
pub async fn discover_meta_set(paths: &[String]) -> Result<MetaSetDiscovery> {
    let obs = observe_meta_set(paths).await?;
    let refuse = |msg: String| Err(crate::error::SqueezefsError::InvalidOperation(msg));

    let stamped_count = obs.iter().filter(|o| o.stamp.is_some()).count();
    if stamped_count == 0 {
        // Defense in depth (design-dynamic-meta-routing §5.6): every
        // dynamic-routing format stamps its bootstrap ledger record, and
        // the superblock gate already refused pre-dynamic-routing
        // volumes before this point — an all-stampless bit-6 set means
        // torn formats or foreign ledgers, never a mountable legacy set
        // (the pre-campaign implicit-identity arm died with the frozen
        // widths it served).
        let listed: Vec<&str> = obs.iter().map(|o| o.path.as_str()).collect();
        return refuse(format!(
            "metadata volumes {listed:?} carry NO §5.5.1a membership stamps — every \
             dynamic-routing format stamps its members at format time, so these ledgers \
             are torn or foreign; reformat required (`squeezefs format --force`, destroys \
             the old contents)"
        ));
    }
    if stamped_count < obs.len() {
        let stampless: Vec<&str> = obs
            .iter()
            .filter(|o| o.stamp.is_none())
            .map(|o| o.path.as_str())
            .collect();
        return refuse(format!(
            "metadata set mixes stamped and stampless members: {} of {} volumes carry a \
             §5.5.1a membership stamp but {stampless:?} carry none — a slot-mapped set's \
             members are all stamped; if a repair was interrupted, run \
             `squeezefs volume repair-set <sqmeta-uri>`",
            stamped_count,
            obs.len()
        ));
    }

    // All stamped: cross-check identity + geometry, then resolve the
    // §5.5.2b states. Epochs may legitimately DIFFER at rest (a slot
    // flip bumps only its participants): per-slot highest-epoch-wins
    // resolves dual claims; a same-epoch dual claim is corruption
    // (refuse loud). Member-count disagreement = an in-progress
    // MEMBERSHIP change (add/remove crash window) — refuse naming the
    // idempotent re-run. Tombstones (member_count == 0) are retired
    // members and refuse loud.
    let name = |o: &MetaVolumeObservation| o.path.clone();
    if let Some(tomb) = obs
        .iter()
        .find(|o| o.stamp.as_ref().is_some_and(|st| st.member_count == 0))
    {
        return refuse(format!(
            "metadata volume {} is a RETIRED member (remove-meta tombstone) — remove it \
             from the URI; it carries no live slots",
            name(tomb)
        ));
    }
    let first = obs[0].stamp.as_ref().expect("all stamped");
    for o in &obs[1..] {
        let st = o.stamp.as_ref().expect("all stamped");
        if st.set_uuid != first.set_uuid {
            return refuse(format!(
                "metadata volumes belong to DIFFERENT sets: {} and {} carry different \
                 set uuids ({:02x?} vs {:02x?}) — one of them is from another filesystem",
                name(&obs[0]),
                name(o),
                &first.set_uuid[..4],
                &st.set_uuid[..4],
            ));
        }
        if st.routing_width != first.routing_width {
            return refuse(format!(
                "metadata set stamps disagree on the frozen routing width: {} declares \
                 {} while {} declares {}",
                name(&obs[0]),
                first.routing_width,
                name(o),
                st.routing_width,
            ));
        }
        if st.member_count != first.member_count {
            return refuse(format!(
                "metadata set stamps disagree on membership (an interrupted \
                 add-meta/remove-meta): {} declares {} members while {} declares {} — \
                 re-run the interrupted `squeezefs volume add-meta`/`remove-meta` with \
                 the same arguments (idempotent), or `squeezefs volume repair-set`",
                name(&obs[0]),
                first.member_count,
                name(o),
                st.member_count,
            ));
        }
    }
    let member_count = usize::from(first.member_count);
    if member_count != obs.len() {
        let listed: Vec<&str> = obs.iter().map(|o| o.path.as_str()).collect();
        return refuse(format!(
            "metadata set stamps declare {member_count} members but the URI lists {} \
             ({listed:?}) — a member is {}; every member of the set must be listed",
            obs.len(),
            if member_count > obs.len() {
                "missing from the URI"
            } else {
                "listed that the stamps do not name"
            },
        ));
    }

    // Canonical ordering: ascending member_position (positions are
    // unique but may be SPARSE after a remove-meta — survivors keep
    // their positions; only uniqueness is structural). Duplicates refuse
    // naming BOTH volumes.
    let mut ordered: Vec<&MetaVolumeObservation> = obs.iter().collect();
    ordered.sort_by_key(|o| o.stamp.as_ref().expect("all stamped").member_position);
    for pair in ordered.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let pa = a.stamp.as_ref().expect("all stamped").member_position;
        if pa == b.stamp.as_ref().expect("all stamped").member_position {
            return refuse(format!(
                "metadata volumes {} and {} BOTH stamp member position {pa} — duplicate \
                 membership; one of them is stale or foreign",
                name(a),
                name(b)
            ));
        }
    }

    // Slot map: per-slot highest-epoch-wins over the claims (§5.5.2b).
    let width = first.routing_width as u64;
    let mut slot_claims: Vec<Option<(usize, u64)>> = vec![None; first.routing_width as usize];
    for (vol_idx, o) in ordered.iter().enumerate() {
        let st = o.stamp.as_ref().expect("all stamped");
        for slot in st.slots_hosted.iter() {
            let s = usize::from(slot);
            if s >= slot_claims.len() {
                return refuse(format!(
                    "metadata volume {} hosts slot {slot} outside the frozen routing \
                     width {width}",
                    name(o)
                ));
            }
            match slot_claims[s] {
                None => slot_claims[s] = Some((vol_idx, st.set_epoch)),
                Some((prev_idx, prev_epoch)) => {
                    if prev_epoch == st.set_epoch {
                        return refuse(format!(
                            "metadata volumes {} and {} BOTH host routing slot {slot} at \
                             the SAME epoch {prev_epoch} — impossible by construction \
                             (one coordinator, one flip per slot); treat as corruption",
                            name(ordered[prev_idx]),
                            name(o)
                        ));
                    }
                    // The §5.5.2b dual-claim window: the HIGHER epoch's
                    // claim wins (target-first ⇒ the winner's copy is
                    // complete before it may ever claim).
                    if st.set_epoch > prev_epoch {
                        slot_claims[s] = Some((vol_idx, st.set_epoch));
                    }
                }
            }
        }
    }
    let mut map = Vec::with_capacity(slot_claims.len());
    for (slot, claim) in slot_claims.into_iter().enumerate() {
        match claim {
            Some((v, _)) => map.push(v),
            None => {
                return refuse(format!(
                    "routing slot {slot} of width {width} is hosted by NO listed volume — \
                     the durable slot map is incomplete; run `squeezefs volume repair-set`"
                ));
            }
        }
    }
    let native_slots: Vec<Option<u16>> = ordered
        .iter()
        .map(|o| {
            o.stamp
                .as_ref()
                .expect("all stamped")
                .resolved_native_slot()
        })
        .collect();
    let set_epoch = obs
        .iter()
        .map(|o| o.stamp.as_ref().expect("all stamped").set_epoch)
        .max()
        .unwrap_or(0);

    Ok(MetaSetDiscovery {
        ordered_paths: ordered.iter().map(|o| o.path.clone()).collect(),
        uuids: ordered.iter().map(|o| o.uuid).collect(),
        routing_width: width,
        slot_to_volume: map,
        native_slots,
        set_epoch,
    })
}

/// The format-time slot plan (design-dynamic-meta-routing §5.1): one
/// fresh set uuid, epoch 1, the identity slot distribution
/// (`slot k → member position k mod N` — exactly ONE stride run per
/// member; every member's native mint slot is its own position), and
/// one membership stamp per member, over the DERIVED routing width.
#[derive(Debug, Clone)]
pub struct MetaSlotPlan {
    /// The set identity every member's stamp carries.
    pub set_uuid: [u8; 16],
    /// The derived routing width W.
    pub routing_width: u32,
    /// Per-member stamps, indexed by `member_position`.
    pub stamps: Vec<kv::checkpoint::MembershipStamp>,
}

/// Build the [`MetaSlotPlan`] for `volume_count` members over the
/// DERIVED width ([`DERIVED_ROUTING_WIDTH`] — never a knob; the retired
/// `format --meta-slots` refuses loud naming this derivation).
pub fn plan_meta_slot_set(volume_count: usize) -> Result<MetaSlotPlan> {
    plan_meta_slot_set_with_width(volume_count, DERIVED_ROUTING_WIDTH)
}

/// [`plan_meta_slot_set`]'s width-parametric core. Production formats
/// always pass [`DERIVED_ROUTING_WIDTH`]; small explicit widths remain
/// valid ROUTING geometries and are the geometry-test surface
/// (`tests/meta_slot_tests.rs` exercises flip/discovery/refusal shapes
/// at W = 8 where every slot is enumerable by hand).
pub fn plan_meta_slot_set_with_width(volume_count: usize, width: u32) -> Result<MetaSlotPlan> {
    let n = u32::try_from(volume_count).map_err(|_| {
        crate::error::SqueezefsError::InvalidOperation(format!(
            "absurd meta volume count {volume_count}"
        ))
    })?;
    if n == 0 {
        return Err(crate::error::SqueezefsError::InvalidOperation(
            "at least one metadata volume is required".to_string(),
        ));
    }
    if width < n {
        return Err(crate::error::SqueezefsError::InvalidOperation(format!(
            "routing width {width} below the volume count {n}: every metadata volume \
             must host at least one slot"
        )));
    }
    if width > DERIVED_ROUTING_WIDTH {
        return Err(crate::error::SqueezefsError::InvalidOperation(format!(
            "routing width {width} exceeds the u16 slot-id namespace \
             ({DERIVED_ROUTING_WIDTH})"
        )));
    }
    let set_uuid = *uuid::Uuid::new_v4().as_bytes();
    let stamps = (0..n)
        .map(|pos| {
            // Member pos hosts {pos + k·n | pos + k·n < width}: one
            // stride run (singletons canonicalize to stride 1 — a
            // 65536-member set's stride would overflow u16).
            let count = (width - pos).div_ceil(n);
            let run = kv::slot_set::SlotRun {
                start: pos as u16,
                stride: if count == 1 { 1 } else { n as u16 },
                count,
            };
            Ok(kv::checkpoint::MembershipStamp {
                set_uuid,
                set_epoch: 1,
                member_position: pos as u16,
                member_count: n as u16,
                routing_width: width,
                slots_hosted: kv::slot_set::SlotSet::from_runs(vec![run])
                    .map_err(crate::error::SqueezefsError::from)?,
                native_slot: Some(pos as u16),
                slot_cursors: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(MetaSlotPlan {
        set_uuid,
        routing_width: width,
        stamps,
    })
}

/// The mounted volume set's **filesystem generation identity** — what a
/// `squeezefs format` invocation changes and nothing else does. Local NVMe
/// staging is bound to this identity (`cache::nvme` stamps it into every
/// staging dir and discards staging content stamped by a DEAD generation
/// before any recovery/seeding runs — the reformat-over-stale-staging
/// poisoning fix).
///
/// Per-volume identity: the v3 superblock `uuid`
/// (`kv::superblock::SuperblockV3::uuid`) — random at every format
/// ([`kv::builder::BuilderConfig::new`]), read straight off sector 0
/// without mounting the volume.
///
/// The set identity is the ORDERED join of per-volume identities in
/// **canonical set order** ([`discover_meta_set`]): `route_ino` stripes
/// by that order, so a genuinely reordered LEGACY set is a different
/// metadata view and must not adopt the old set's staging — while a
/// STAMPED set's canonical order rides `member_position`, making the
/// generation identical under any operator URI ordering (§5.5.1a).
/// The bit-11 uniformity half of the KD-MW-1 writable-mount refusal
/// predicate (design-full-multi-writer §6.2 mechanism ii), over the set's
/// CANONICAL path order. Refuses iff:
///
/// * any volume carries bit 11 (`KV_MULTI_WRITER_DATA`) **without the
///   other eight** [`kv::superblock::MULTI_WRITER_FORMAT_BITS`] members —
///   the enable verb stamps bit 11 TERMINAL per volume by construction,
///   so this state can only come from a foreign tool or corruption
///   (refused naming fsck, never auto-repaired);
/// * bit-11 presence **differs across the set** (shape (a) — a crash
///   between volumes), refused naming the lagging volume(s) and the
///   resume remedy.
///
/// Shape (c) — legitimately partial populations NOT including bit 11
/// (standalone bit 7, Phase-8 bit-15 stamps, bit-5 runtime stamps, the
/// pre-mw six-bit test populations MINUS bit 11, …) — never trips either
/// arm: those are exactly today's legal field states, grandfathered.
pub async fn refuse_mixed_multi_writer_set(ordered_paths: &[String]) -> Result<()> {
    use kv::superblock::{
        FEATURE_INCOMPAT_KV_MULTI_WRITER_DATA as BIT11, MULTI_WRITER_FORMAT_BITS as ALL_NINE,
    };
    let mut mw_volumes: Vec<&str> = Vec::new();
    let mut lagging: Vec<&str> = Vec::new();
    for path in ordered_paths {
        let features = match kv::superblock::classify_volume(std::path::Path::new(path)).await? {
            kv::superblock::VolumeFormat::V3(sb) => sb.features_incompat,
            // Blank/legacy volumes fail the open gate downstream with
            // their own precise errors; the mw predicate has nothing
            // to say about them.
            _ => continue,
        };
        if features & BIT11 != 0 {
            if features & ALL_NINE != ALL_NINE {
                return Err(crate::error::SqueezefsError::InvalidOperation(format!(
                    "refusing a writable mount: metadata volume {path} carries the \
                     multi-writer data-plane bit (11) WITHOUT the other eight \
                     multi-writer format bits — the enable verb stamps bit 11 last \
                     by construction, so this is a foreign-tool or corruption \
                     state. Run `squeezefs fsck` on the volume; no automatic \
                     repair is attempted (features {features:#x})"
                )));
            }
            mw_volumes.push(path);
        } else {
            lagging.push(path);
        }
    }
    if !mw_volumes.is_empty() && !lagging.is_empty() {
        return Err(crate::error::SqueezefsError::InvalidOperation(format!(
            "refusing a writable mount: bit-11 (multi-writer) presence differs \
             across the metadata set — {mw:?} are multi-writer-capable while \
             {lag:?} lag (a `squeezefs volume enable-multi-writer` run crashed \
             between volumes). Re-run `squeezefs volume enable-multi-writer \
             <sqmeta-uri>` (idempotent) to converge; read-only mounts keep \
             serving",
            mw = mw_volumes,
            lag = lagging,
        )));
    }
    Ok(())
}

pub async fn volume_set_generation(meta_lvs: &[String]) -> Result<String> {
    let disc = discover_meta_set(meta_lvs).await?;
    Ok(generation_from_uuids(&disc.uuids))
}

/// The generation string for an ordered superblock-uuid list — the join
/// [`volume_set_generation`] performs after discovery. Public so the
/// §5.5.2b add-meta resume path (VL8 item 8) can recompute the OLD set's
/// generation when a crashed coordinator left the old URI refusing
/// discovery (stamped-ahead membership counts).
pub fn generation_from_uuids(uuids: &[[u8; 16]]) -> String {
    use std::fmt::Write as _;
    let mut parts = Vec::with_capacity(uuids.len());
    for uuid in uuids {
        let mut s = String::with_capacity(3 + 32);
        s.push_str("v3:");
        for b in uuid {
            let _ = write!(s, "{b:02x}");
        }
        parts.push(s);
    }
    parts.join("|")
}

#[cfg(test)]
mod knob_tests {
    use super::*;

    /// Knob resolution (design-wal-crash-consistency §4.2): canonical
    /// `SQUEEZEFS_META_FLUSH_INTERVAL_MS` wins over the legacy
    /// `SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS` alias; the alias alone still
    /// works; default is 50 ms. (Serial gate: env is process-global.)
    #[test]
    fn test_flush_interval_env_alias_precedence() {
        const NEW: &str = "SQUEEZEFS_META_FLUSH_INTERVAL_MS";
        const OLD: &str = "SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS";
        let saved = (std::env::var(NEW).ok(), std::env::var(OLD).ok());

        std::env::remove_var(NEW);
        std::env::remove_var(OLD);
        assert_eq!(resolve_flush_interval_ms(), 50, "default is 50 ms");

        std::env::set_var(OLD, "7");
        assert_eq!(resolve_flush_interval_ms(), 7, "legacy alias honored");

        std::env::set_var(NEW, "13");
        assert_eq!(
            resolve_flush_interval_ms(),
            13,
            "canonical name wins over alias"
        );

        match saved.0 {
            Some(v) => std::env::set_var(NEW, v),
            None => std::env::remove_var(NEW),
        }
        match saved.1 {
            Some(v) => std::env::set_var(OLD, v),
            None => std::env::remove_var(OLD),
        }
    }
}
