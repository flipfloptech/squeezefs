//! Writer-scoped staging identity — pre-RC engineering spec §6.2 **items
//! 8 and 10**, under ruling **D9** (the bit is BUILT, never STAMPED).
//!
//! Two durable single-writer assumptions live here, and they are two
//! halves of ONE contract:
//!
//! * **Item 8** — `active_block:` / `active_block_ext:` / `mapping:` keys
//!   have no writer scope. They are SHARED keys naming node-PRIVATE
//!   staging payloads: two writers collide on one key, and recovery
//!   cannot classify a record it did not write.
//! * **Item 10** — the staging generation stamp is the volume-set uuids
//!   only (`meta_backend::volume_set_generation`), i.e. **identical on
//!   every node**. It was designed to catch reformats, not peers, so
//!   node B's staged payloads pass node A's generation gate.
//!
//! The record-level half (a key component) classifies one RECORD; the
//! root-level half (the stamp) classifies one staging ROOT. Both engage
//! from the SAME token and the SAME superblock bit
//! ([`crate::meta_backend::kv::superblock::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING`]),
//! because a half-engaged state is unsound in both directions: scoped
//! keys under a node-blind root gate still let a peer's root be adopted
//! wholesale, and a node-scoped root whose records are unlabelled cannot
//! classify the records inside it.
//!
//! # The scope token is the NODE, not the mount and not the writer term
//!
//! Staging payloads are **node-local by construction** — the bytes live
//! in this host's staging ring. So the only scope granularity that can
//! answer "is this record's payload reachable by me?" is the node:
//!
//! * a per-MOUNT identity (the D0 [`crate::meta_backend::kv::backend::
//!   WriterClaim`]`::id`, a fresh uuid per mount) would make the
//!   successor mount classify its own predecessor's crash residue as
//!   FOREIGN — the staged-crash-recovery contract (`tests/
//!   staged_crash_recovery_tests.rs`) inverted into data loss;
//! * a boot-scoped identity (`WriterClaim.boot`, `/proc/sys/kernel/
//!   random/boot_id`) would orphan every legitimate staged payload at
//!   the first reboot;
//! * the durable writer TERM (incompat bit 7) is currency, not identity:
//!   it belongs in record VALUES (`StagedMetadata.fencing_token`,
//!   `ExtentRecord.fencing_token`), where the remount law already reads
//!   it. Keys need identity; values carry currency.
//!
//! Hence: **host-stable, reboot-stable, process- and user-independent,
//! distinct across hosts.** `/etc/machine-id` is exactly that contract
//! (systemd: generated at install, stable for the installation's
//! lifetime), with `/var/lib/dbus/machine-id` and an operator-provided
//! `/etc/squeezefs/node-id` as fallbacks. Process independence is
//! load-bearing for KD-8: the OFFLINE `volume add-meta` /
//! `remove-meta` verbs rebind the staging generation from a different
//! process (often a different uid, under sudo) than the daemon that
//! mounts afterwards — if the two derived different tokens, the rebind
//! would write a stamp the next mount rejects, and durable acked staged
//! payloads would be discarded. That is a data-loss path, pinned in
//! `tests/writer_scoped_staging_tests.rs`.
//!
//! The raw machine-id is never written to disk: the token is the
//! **app-specific derivation** `xxh3_64(machine_id, NODE_SCOPE_APP_SEED)`
//! (systemd's own `sd_id128_get_machine_app_specific` discipline), which
//! also gives a fixed 16-hex rendering in the house `vol-{16 hex}` style.
//!
//! # The scope is the PAIR `(node_token, mount_slot)` — KD-MW-2
//!
//! `docs/design-full-multi-writer.md` §5.1: same-machine multi-mount
//! clients make the node alone too coarse — two co-located mounts of one
//! stamped set would share one scope, so writer-scoped staging could not
//! classify their records apart and each would adopt the other's roots.
//! The client identity is therefore the pair:
//!
//! * `node_token` — the unchanged ladder above;
//! * `mount_slot` — `xxh3_64(canonicalized mount point)` truncated to
//!   32 bits ([`derive_mount_slot`]). **Mount-point-stable** (a restart of
//!   the same mount point is the SAME client — the staged-crash-recovery
//!   argument above, unchanged), **distinct across co-located mounts**.
//!   Operator override: `-o client_slot=<hex8>`
//!   ([`client_slot_from_options`]) for mount-point-migration cases.
//!
//! `slot == 0` is the **node-only** scope: the identity of every OFFLINE
//! process (the KD-8 `add-meta`/`remove-meta` rebind verbs, fsck, the
//! §5.1(b) `squeezefs staging adopt` verb — none of them has a mount
//! point), and the grandfathered form every pre-pair binary minted. A
//! node-only marker under a pair-scoped mount is the upgrade arm
//! ([`GenerationBinding::ScopeUpgrade`]); a node-only IDENTITY over this
//! node's pair-scoped roots is rebindable for the same reason the
//! un-scoped binding is (the offline verbs must carry every one of this
//! node's roots across a membership change, or the next mount's
//! dead-generation discard destroys acked staged payloads).
//!
//! The one hazard mount-point-stable identity introduces — remounting the
//! same set at a DIFFERENT path makes the successor a different client,
//! stranding the predecessor's staged residue (possibly acked custody) —
//! is the **moved-mount-point law** (design §5.1, crash window MW-1b):
//! detected and reported LOUD at every mount by
//! [`crate::config_ops::scan_scoped_staging_siblings`], listed by
//! `squeezefs clients`, resolved by `-o client_slot=<hex8>` (adopt at
//! mount) or `squeezefs staging adopt|discard --slot <hex8>` — never
//! silently stranded.

use crate::error::{Result, SqueezefsError};

/// Domain-separation seed for the app-specific node-token derivation (the
/// raw machine-id is treated as confidential and never lands on disk).
const NODE_SCOPE_APP_SEED: u64 = u64::from_be_bytes(*b"SQZNODE1");

/// Reserved token: `0` means "no scope" everywhere in this module, so a
/// derivation that lands on 0 is nudged to 1.
const NO_SCOPE: u64 = 0;

/// Key component prefix: keys carry `…:w_{16 hex}` (node-only scope) or
/// `…:w_{16 hex}.m{8 hex}` (pair scope, KD-MW-2) as their LAST component
/// (see [`scoped_key_suffix`]).
const KEY_SCOPE_TAG: &str = ":w_";

/// Mount-slot tag inside a pair-scoped key suffix / generation
/// decoration: `.m{8 hex}` after the 16-hex node token.
const MOUNT_SLOT_TAG: &str = ".m";

/// Rendered node-only key-scope suffix length: `":w_"` + 16 hex.
const KEY_SCOPE_NODE_LEN: usize = KEY_SCOPE_TAG.len() + 16;

/// Rendered pair key-scope suffix length: `":w_"` + 16 hex + ".m" + 8 hex.
const KEY_SCOPE_PAIR_LEN: usize = KEY_SCOPE_NODE_LEN + MOUNT_SLOT_TAG.len() + 8;

/// Staging-generation decoration: `{set_generation}@node:{16 hex}` or
/// `{set_generation}@node:{16 hex}.m{8 hex}` (the pair form).
const GENERATION_SCOPE_TAG: &str = "@node:";

/// Where a node identity came from (logged at mount; part of the
/// operator's story when a scope refusal fires).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeIdSource {
    /// `SQUEEZEFS_NODE_ID_FILE` (explicit-wins, the env-knob precedence
    /// law; also the suites' two-node seam).
    EnvFile,
    /// `/etc/machine-id` (systemd; the expected source).
    MachineId,
    /// `/var/lib/dbus/machine-id` (pre-systemd hosts).
    DbusMachineId,
    /// `/etc/squeezefs/node-id` (operator-provided last resort).
    SqueezefsNodeId,
}

impl NodeIdSource {
    pub fn path(&self) -> &'static str {
        match self {
            NodeIdSource::EnvFile => "SQUEEZEFS_NODE_ID_FILE",
            NodeIdSource::MachineId => "/etc/machine-id",
            NodeIdSource::DbusMachineId => "/var/lib/dbus/machine-id",
            NodeIdSource::SqueezefsNodeId => "/etc/squeezefs/node-id",
        }
    }
}

/// A resolved node identity: the app-specific token plus its provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeIdentity {
    pub token: u64,
    pub source: NodeIdSource,
}

/// The client scope (KD-MW-2): `(node_token, mount_slot)`.
///
/// `slot == 0` is the **node-only** scope — the offline-process identity
/// and the grandfathered pre-pair form (module docs). A mount's slot is
/// never 0 ([`derive_mount_slot`] nudges, [`client_slot_from_options`]
/// refuses it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WriterScope {
    /// The app-specific node token ([`resolve_node_identity`]).
    pub node: u64,
    /// The mount slot (`0` = node-only).
    pub slot: u32,
}

impl WriterScope {
    /// The pair scope of a mount.
    pub const fn new(node: u64, slot: u32) -> Self {
        Self { node, slot }
    }

    /// The node-only scope (offline processes, pre-pair grandfathering).
    pub const fn node_only(node: u64) -> Self {
        Self { node, slot: 0 }
    }

    /// `true` ⇔ this is the node-only form.
    pub const fn is_node_only(&self) -> bool {
        self.slot == 0
    }

    /// The operator-facing rendering: `w_{16 hex}` or
    /// `w_{16 hex}.m{8 hex}` — the same spelling the key suffix and the
    /// generation decoration carry (minus their tags), so a refusal, the
    /// stats inode and a key all name one scope one way.
    pub fn render(&self) -> String {
        match self.slot {
            0 => format!("w_{:016x}", self.node),
            slot => format!("w_{:016x}{MOUNT_SLOT_TAG}{slot:08x}", self.node),
        }
    }
}

impl std::fmt::Display for WriterScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render())
    }
}

/// Domain-separation seed for the mount-slot derivation (design §5.1: the
/// slot is `xxh3_64(canonicalized mount point)` truncated to 32 bits —
/// its own domain, so a mount point and a machine-id can never alias).
const MOUNT_SLOT_APP_SEED: u64 = u64::from_be_bytes(*b"SQZSLOT1");

/// Derive a mount slot from the CANONICALIZED mount point (design §5.1).
///
/// Mount-point-stable by construction (same canonical path ⇒ same slot —
/// the crash-successor-adopts-its-own-residue law), distinct across
/// co-located mounts (different paths hash apart; an observed collision
/// within one claim set refuses the mount loud — OQ-5's resolved form —
/// and `-o client_slot=` is the remedy). `0` is the reserved node-only
/// sentinel, so a derivation landing there is nudged to 1, exactly as
/// [`derive_node_token`] nudges.
pub fn derive_mount_slot(canonical_mount_point: &str) -> u32 {
    let h =
        xxhash_rust::xxh3::xxh3_64_with_seed(canonical_mount_point.as_bytes(), MOUNT_SLOT_APP_SEED);
    match (h & 0xFFFF_FFFF) as u32 {
        0 => 1,
        slot => slot,
    }
}

/// Parse a `<hex8>` mount-slot spelling (the `-o client_slot=` value and
/// the `staging adopt|discard --slot` argument): 1–8 hex digits, nonzero.
///
/// A malformed value is a REFUSAL, never a fallback (the env-knob value
/// law, applied to the one identity-bearing mount option): the slot
/// classifies staged write custody, and a silently-defaulted one either
/// strands the residue the operator is trying to adopt or adopts the
/// wrong client's. (Contrast `admin_uid_from_options`, whose fallback is
/// a fail-SAFE — never widening to root; a wrong identity has no safe
/// direction to fall back to.)
pub fn parse_client_slot(value: &str) -> Result<u32> {
    let v = value.trim();
    let refusal = |detail: &str| {
        SqueezefsError::InvalidOperation(format!(
            "client_slot '{value}' is not a mount slot ({detail}): expected <hex8> — 1–8 hex \
             digits, nonzero — the `m{{8 hex}}` value a residue report / `squeezefs clients` \
             prints. Refusing rather than defaulting: the slot classifies staged write \
             custody, and a wrong one either strands acked staged payloads or adopts another \
             client's"
        ))
    };
    if v.is_empty() || v.len() > 8 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(refusal("must be 1–8 hex digits"));
    }
    let slot = u32::from_str_radix(v, 16).map_err(|_| refusal("unparseable hex"))?;
    if slot == 0 {
        return Err(refusal(
            "0 is the reserved node-only sentinel and never names a mount",
        ));
    }
    Ok(slot)
}

/// The `-o client_slot=<hex8>` mount-option override (design §5.1 / §11):
/// the explicit mount-slot for mount-point-migration cases — the remedy
/// the moved-mount-point residue report names. `Ok(None)` when absent;
/// a malformed value REFUSES the mount ([`parse_client_slot`]'s law).
pub fn client_slot_from_options(opts: Option<&str>) -> Result<Option<u32>> {
    let Some(opts) = opts else {
        return Ok(None);
    };
    for opt in opts.split(',') {
        if let Some(v) = opt.trim().strip_prefix("client_slot=") {
            return parse_client_slot(v).map(Some);
        }
    }
    Ok(None)
}

/// The pure derivation: an identity source's raw bytes → the
/// app-specific node token. `None` for material that cannot identify a
/// host — empty/whitespace, shorter than 8 significant bytes, or the
/// all-zero "uninitialized" machine-id systemd writes on a firstboot it
/// could not commit.
pub fn derive_node_token(raw: &[u8]) -> Option<u64> {
    let trimmed: &[u8] = {
        let start = raw.iter().position(|b| !b.is_ascii_whitespace())?;
        let end = raw.iter().rposition(|b| !b.is_ascii_whitespace())? + 1;
        &raw[start..end]
    };
    if trimmed.len() < 8 {
        return None;
    }
    if trimmed.iter().all(|b| *b == b'0') {
        // systemd's uninitialized value: identical on every host that
        // has one, which is the exact opposite of an identity.
        return None;
    }
    let token = xxhash_rust::xxh3::xxh3_64_with_seed(trimmed, NODE_SCOPE_APP_SEED);
    Some(if token == NO_SCOPE { 1 } else { token })
}

/// The source ladder, in precedence order (explicit wins — the ONE
/// env-knob convention). Reads are plain `std::fs` by design: this runs
/// once per process before any ring exists, on files in `/etc`.
fn node_id_sources() -> Vec<(NodeIdSource, std::path::PathBuf)> {
    let mut out = Vec::with_capacity(4);
    // The ONE env-knob convention's absence rule (ENG-10): unset, empty
    // and whitespace-only are all "absent".
    if let Some(p) = std::env::var("SQUEEZEFS_NODE_ID_FILE")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        out.push((NodeIdSource::EnvFile, std::path::PathBuf::from(p)));
    }
    out.push((NodeIdSource::MachineId, "/etc/machine-id".into()));
    out.push((
        NodeIdSource::DbusMachineId,
        "/var/lib/dbus/machine-id".into(),
    ));
    out.push((
        NodeIdSource::SqueezefsNodeId,
        "/etc/squeezefs/node-id".into(),
    ));
    out
}

/// Resolve this host's node identity, or refuse LOUD.
///
/// Refusal is the right posture, not a synthesized fallback: the token is
/// a correctness input to record classification, and an unstable or
/// colliding one would either strand our own acked staged payloads or
/// adopt a peer's. A refusal names every source it tried and the remedy.
/// Nothing calls this unless a volume set carries the incompat bit, so no
/// shipped mount can reach it (ruling D9 — nothing stamps).
pub fn resolve_node_identity() -> Result<NodeIdentity> {
    let sources = node_id_sources();
    for (source, path) in &sources {
        let Ok(raw) = std::fs::read(path) else {
            continue;
        };
        if let Some(token) = derive_node_token(&raw) {
            return Ok(NodeIdentity {
                token,
                source: *source,
            });
        }
        log::warn!(
            "node identity source {} ({}) holds no usable host identity \
             (empty, too short, or the all-zero uninitialized value) — trying the next source",
            source.path(),
            path.display()
        );
    }
    Err(SqueezefsError::InvalidOperation(format!(
        "this volume set is writer-scoped (superblock incompat bit \
         {}) but no stable node identity could be resolved: tried {}. \
         A node identity must survive reboots and differ across hosts — \
         remedy: `systemd-machine-id-setup`, or write 8+ stable bytes to \
         /etc/squeezefs/node-id (or point SQUEEZEFS_NODE_ID_FILE at such \
         a file). Refusing rather than guessing: the token classifies \
         staged write custody, and a wrong one either strands this node's \
         acked staged payloads or adopts a peer's",
        crate::meta_backend::kv::superblock::WRITER_SCOPED_STAGING_BIT,
        sources
            .iter()
            .map(|(s, p)| format!("{} ({})", s.path(), p.display()))
            .collect::<Vec<_>>()
            .join(", "),
    )))
}

/// Does every member of `meta_lvs` carry the writer-scoped-staging
/// incompat bit? Unanimity is required: a partially stamped set is NOT
/// scoped (the never-trust-a-partial-population rule — a half-scoped set
/// would label some records and not others, which is strictly worse than
/// labelling none).
///
/// `Ok(None)` = disengaged, i.e. EXACTLY today's behavior.
/// `Ok(Some(scope))` = scoped, answered as the NODE-ONLY scope — the
/// set-level question is node-scoped-ness; a MOUNT composes its mount
/// slot on top (KD-MW-2, `WriterScope { node, slot }`), while the
/// offline verbs use this answer verbatim (their identity IS the node).
/// The superblock probe is the same 4 KiB sector-0 read
/// `volume_set_generation` performs, deliberately repeated here rather
/// than threaded through `MetaSetDiscovery` so this stays one
/// self-contained mount-path function.
pub async fn resolve_scope_for_set(meta_lvs: &[String]) -> Result<Option<WriterScope>> {
    use crate::meta_backend::kv::superblock::{
        classify_volume, VolumeFormat, FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING,
    };
    if meta_lvs.is_empty() {
        return Ok(None);
    }
    for path in meta_lvs {
        match classify_volume(std::path::Path::new(path)).await? {
            VolumeFormat::V3(sb)
                if sb.features_incompat & FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING != 0 => {}
            // Blank / legacy-v2 / unstamped: disengaged. The mount's own
            // gates own those refusals; this surface only classifies.
            _ => return Ok(None),
        }
    }
    Ok(Some(WriterScope::node_only(resolve_node_identity()?.token)))
}

// ---------------------------------------------------------------------------
// Process-global engagement (mount-time, once)
// ---------------------------------------------------------------------------

/// The process's scope node token; `0` = disengaged (a derived token is
/// never 0 — see [`derive_node_token`]).
///
/// Relaxed atomic words, deliberately: key minting is on the small-write
/// hot path, where the house law is latch-free. A `RwLock` here measured
/// 11× on the mount-path classification sweep and would serialize every
/// writer's key mint on one cache line (`benches/write_path_bench.rs`
/// `writer_scope` group). The node word is the GATE (0 = disengaged), so
/// [`engage`] stores the slot before it and readers load it first — and
/// engagement is mount-time-once, before any op is served, so no reader
/// ever races the pair.
static SCOPE_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(NO_SCOPE);

/// The engaged scope's mount slot (KD-MW-2; `0` = node-only scope).
static SCOPE_SLOT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// This process's mount slot, engaged or not — the CLIENT-identity half
/// (KD-MW-2) that `cowriter::node_member_id` and the membership join
/// carry even while the staging scope itself is decided by incompat
/// bit 10. `0` = no mount (offline verbs, tests).
static MOUNT_SLOT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The canonicalized mount point (set once per mount, with the slot):
/// what the membership join reports so an OQ-5 slot-collision refusal can
/// name BOTH colliding mount points.
static MOUNT_POINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record this mount's client-identity half (KD-MW-2): the mount slot and
/// the canonicalized mount point it was derived from (or overridden for).
/// Called ONCE per process from the mount path, before any plane joins —
/// and before the named thread populations spawn, because it also seeds
/// the per-mount comm suffix (design-full-multi-writer §5.3, PR 3) into
/// EVERY crate copy of `comm_core`: squeezefs-ipc's real-dependency
/// static (the root crate's own sites resolve through it) and the fuse3
/// fork's `#[path]`-included copy (its own static, invisible to the
/// former — the production-sharing precedent's one cost).
pub fn set_mount_identity(slot: u32, canonical_mount_point: &str) {
    MOUNT_SLOT.store(slot, std::sync::atomic::Ordering::Release);
    squeezefs_ipc::comm_core::set_comm_tag(slot);
    fuse3::comm_core::set_comm_tag(slot);
    let _ = MOUNT_POINT.set(canonical_mount_point.to_string());
}

/// This process's mount slot (`0` = no mount — offline verbs, tests).
#[inline]
pub fn mount_slot() -> u32 {
    MOUNT_SLOT.load(std::sync::atomic::Ordering::Relaxed)
}

/// The canonicalized mount point, when this process is a mount.
pub fn mount_point() -> Option<&'static str> {
    MOUNT_POINT.get().map(String::as_str)
}

/// Engage (or, with `None`, disengage) the process-global writer scope.
/// Called ONCE per process from the mount path after
/// [`resolve_scope_for_set`]; the suites call it directly to drive both
/// arms without touching a superblock.
pub fn engage(scope: Option<WriterScope>) {
    match scope {
        Some(s) => {
            // Slot first, node second: the node word is the readers' gate.
            SCOPE_SLOT.store(s.slot, std::sync::atomic::Ordering::Release);
            SCOPE_TOKEN.store(s.node, std::sync::atomic::Ordering::Release);
        }
        None => {
            SCOPE_TOKEN.store(NO_SCOPE, std::sync::atomic::Ordering::Release);
            SCOPE_SLOT.store(0, std::sync::atomic::Ordering::Release);
        }
    }
}

/// `true` ⇔ this process mints writer-scoped staging keys.
#[inline]
pub fn engaged() -> bool {
    SCOPE_TOKEN.load(std::sync::atomic::Ordering::Relaxed) != NO_SCOPE
}

/// This process's scope, or `None` when disengaged.
#[inline]
pub fn engaged_scope() -> Option<WriterScope> {
    match SCOPE_TOKEN.load(std::sync::atomic::Ordering::Relaxed) {
        NO_SCOPE => None,
        node => Some(WriterScope {
            node,
            slot: SCOPE_SLOT.load(std::sync::atomic::Ordering::Relaxed),
        }),
    }
}

/// A rendered `":w_{16 hex}"` (node-only) or `":w_{16 hex}.m{8 hex}"`
/// (pair) key component — fixed capacity, built on the stack (no global
/// string, no allocation, no lock).
#[derive(Clone, Copy)]
pub struct ScopeSuffix {
    buf: [u8; KEY_SCOPE_PAIR_LEN],
    len: usize,
}

impl ScopeSuffix {
    #[inline]
    fn render(scope: WriterScope) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut buf = [0u8; KEY_SCOPE_PAIR_LEN];
        buf[..KEY_SCOPE_TAG.len()].copy_from_slice(KEY_SCOPE_TAG.as_bytes());
        for i in 0..16 {
            let nibble = (scope.node >> (60 - 4 * i)) & 0xF;
            buf[KEY_SCOPE_TAG.len() + i] = HEX[nibble as usize];
        }
        let mut len = KEY_SCOPE_NODE_LEN;
        if scope.slot != 0 {
            buf[len..len + MOUNT_SLOT_TAG.len()].copy_from_slice(MOUNT_SLOT_TAG.as_bytes());
            len += MOUNT_SLOT_TAG.len();
            for i in 0..8 {
                let nibble = (scope.slot >> (28 - 4 * i)) & 0xF;
                buf[len + i] = HEX[nibble as usize];
            }
            len += 8;
        }
        Self { buf, len }
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        // Only ASCII was written (the tags plus lowercase hex).
        std::str::from_utf8(&self.buf[..self.len]).expect("scope suffix is ASCII")
    }
}

impl std::ops::Deref for ScopeSuffix {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

/// The key suffix to append when minting a staging key: `None` when
/// disengaged (keys stay byte-identical to every shipped release, for the
/// price of one relaxed load and a predicted branch).
#[inline]
pub fn scoped_key_suffix() -> Option<ScopeSuffix> {
    engaged_scope().map(ScopeSuffix::render)
}

// ---------------------------------------------------------------------------
// Key composition and classification (item 8)
// ---------------------------------------------------------------------------

/// Who owns a staging key (the recovery classification, spec §6.2 item 8:
/// *"recovery cannot classify foreign records"*).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOwner {
    /// No scope component: a record written before scoping engaged.
    /// GRANDFATHERED as ours — under the D0 single-writer mount guard
    /// that governed it, this node's daemon is the only writer that could
    /// have produced it, and its payload is in our own staging ring.
    Legacy,
    /// Scope component equals ours — including the node-only form under a
    /// pair scope (a pre-pair binary's record: the same D0 grandfathering
    /// argument, and it is only reachable through a root the generation
    /// gate already adopted via its own upgrade arm).
    Mine,
    /// Scope component names another client — another NODE, or (KD-MW-2)
    /// another MOUNT SLOT of this node (a co-located sibling's record, or
    /// moved-mount-point residue). Never adopted, never flushed, never
    /// freed: the payload lives in THAT client's staging ring, so there
    /// is nothing here we could recover even if custody allowed it.
    Foreign(WriterScope),
    /// Scoped record while WE are unscoped: unprovable ownership. Treated
    /// exactly like [`KeyOwner::Foreign`] (never adopted) — the
    /// declared-unsupported downgrade direction, forward-detected.
    Unprovable(WriterScope),
}

impl KeyOwner {
    /// `true` ⇔ this process may recover, fold, flush and free the record.
    #[inline]
    pub fn is_mine(&self) -> bool {
        matches!(self, KeyOwner::Legacy | KeyOwner::Mine)
    }
}

/// Parse `count` lowercase-hex nibbles into `acc` (uppercase is
/// deliberately NOT accepted: we mint lowercase, so accepting both would
/// make two spellings of one scope).
#[inline]
fn parse_lower_hex(bytes: &[u8]) -> Option<u64> {
    let mut acc: u64 = 0;
    for c in bytes {
        let nibble = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => return None,
        };
        acc = (acc << 4) | nibble as u64;
    }
    Some(acc)
}

/// The scope a key carries, if any: the trailing `:w_{16 lowercase hex}`
/// (node-only) or `:w_{16 hex}.m{8 hex}` (pair) component. Byte-wise +
/// ASCII-only, so it is UTF-8-boundary safe on arbitrary keys and cheap
/// enough for the recovery scan.
///
/// No unscoped key of any shipped release can match: every unscoped form
/// ends in `block_{digits}` or a uuid `file_id`.
#[inline]
pub fn key_scope(key: &str) -> Option<WriterScope> {
    key_scope_len(key).map(|(scope, _)| scope)
}

/// [`key_scope`] plus the matched suffix length (the strip primitive).
#[inline]
fn key_scope_len(key: &str) -> Option<(WriterScope, usize)> {
    let b = key.as_bytes();
    // Pair form first: its own trailing 19 bytes are hex + ".m" + hex,
    // which the node-only probe can never mistake for a `:w_` tag, but
    // the longer match must win so the slot is never silently dropped.
    if b.len() >= KEY_SCOPE_PAIR_LEN {
        let tail = &b[b.len() - KEY_SCOPE_PAIR_LEN..];
        if &tail[..KEY_SCOPE_TAG.len()] == KEY_SCOPE_TAG.as_bytes()
            && &tail[KEY_SCOPE_NODE_LEN..KEY_SCOPE_NODE_LEN + MOUNT_SLOT_TAG.len()]
                == MOUNT_SLOT_TAG.as_bytes()
        {
            if let (Some(node), Some(slot)) = (
                parse_lower_hex(&tail[KEY_SCOPE_TAG.len()..KEY_SCOPE_NODE_LEN]),
                parse_lower_hex(&tail[KEY_SCOPE_NODE_LEN + MOUNT_SLOT_TAG.len()..]),
            ) {
                // A pair spelling of slot 0 is not minted (node-only keys
                // omit the tag), so it must not parse as a scope — two
                // spellings of one scope would be a classification hole.
                if slot != 0 {
                    return Some((WriterScope::new(node, slot as u32), KEY_SCOPE_PAIR_LEN));
                }
                return None;
            }
        }
    }
    if b.len() >= KEY_SCOPE_NODE_LEN {
        let tail = &b[b.len() - KEY_SCOPE_NODE_LEN..];
        if &tail[..KEY_SCOPE_TAG.len()] == KEY_SCOPE_TAG.as_bytes() {
            if let Some(node) = parse_lower_hex(&tail[KEY_SCOPE_TAG.len()..]) {
                return Some((WriterScope::node_only(node), KEY_SCOPE_NODE_LEN));
            }
        }
    }
    None
}

/// A key with its scope component removed — what the historical
/// `strip_prefix(…)` / `split_once(":block_")` parsers must see.
#[inline]
pub fn strip_key_scope(key: &str) -> &str {
    match key_scope_len(key) {
        Some((_, len)) => &key[..key.len() - len],
        None => key,
    }
}

/// Classify a staging key against this process's scope (KD-MW-2: the
/// pair). Same node + same slot = ours; same node + node-only key = ours
/// (pre-pair grandfathering, see [`KeyOwner::Mine`]); any other scoped
/// key — foreign node, a co-located sibling's slot, or a slotted key
/// under a node-only identity (an offline process cannot prove WHICH
/// mount's record it is) — is never ours.
#[inline]
pub fn classify_key(key: &str) -> KeyOwner {
    match (key_scope(key), engaged_scope()) {
        (None, _) => KeyOwner::Legacy,
        (Some(k), None) => KeyOwner::Unprovable(k),
        (Some(k), Some(mine)) => {
            if k.node != mine.node {
                KeyOwner::Foreign(k)
            } else if k.slot == mine.slot || k.slot == 0 {
                KeyOwner::Mine
            } else {
                KeyOwner::Foreign(k)
            }
        }
    }
}

/// `true` ⇔ this process owns `key`'s record ([`KeyOwner::is_mine`]).
#[inline]
pub fn key_is_mine(key: &str) -> bool {
    classify_key(key).is_mine()
}

// ---------------------------------------------------------------------------
// Staging generation decoration (item 10)
// ---------------------------------------------------------------------------

/// The staging generation a staging root is bound to: the volume-set
/// generation, decorated with the node scope when the set is scoped.
///
/// Disengaged output is the set generation VERBATIM — the marker bytes an
/// un-stamped volume writes are byte-identical to every shipped release
/// (pinned in `tests/writer_scoped_staging_tests.rs`).
///
/// The FUSE entry generation (FUSE-4b, `fuse_client::set_entry_generation`)
/// deliberately keeps the UN-decorated set generation: NFS handles name a
/// filesystem, not a node, and folding node identity into them would
/// ESTALE every handle when a set is served from a different host.
pub fn staging_generation(set_generation: &str, scope: Option<WriterScope>) -> String {
    match scope {
        Some(s) if s.slot != 0 => format!(
            "{set_generation}{GENERATION_SCOPE_TAG}{:016x}{MOUNT_SLOT_TAG}{:08x}",
            s.node, s.slot
        ),
        Some(s) => format!("{set_generation}{GENERATION_SCOPE_TAG}{:016x}", s.node),
        None => set_generation.to_string(),
    }
}

/// Split a staging generation into `(set generation, scope)`. An
/// un-decorated string reports the whole value and `None` (so every
/// pre-change marker parses as "this set, no node"), and a garbled
/// decoration degrades the same way — never to a wrong scope.
pub fn split_staging_generation(generation: &str) -> (&str, Option<WriterScope>) {
    let Some(idx) = generation.rfind(GENERATION_SCOPE_TAG) else {
        return (generation, None);
    };
    let (set, tail) = generation.split_at(idx);
    let hex = &tail[GENERATION_SCOPE_TAG.len()..];
    // Node-only form: exactly 16 lowercase hex.
    if hex.len() == 16 {
        if let Some(node) = parse_lower_hex(hex.as_bytes()) {
            return (set, Some(WriterScope::node_only(node)));
        }
        return (generation, None);
    }
    // Pair form (KD-MW-2): 16 hex + ".m" + 8 hex, slot nonzero (a pair
    // spelling of slot 0 is never minted — two spellings of one scope
    // would be a classification hole).
    if hex.len() == 16 + MOUNT_SLOT_TAG.len() + 8 && hex[16..].starts_with(MOUNT_SLOT_TAG) {
        if let (Some(node), Some(slot)) = (
            parse_lower_hex(&hex.as_bytes()[..16]),
            parse_lower_hex(&hex.as_bytes()[16 + MOUNT_SLOT_TAG.len()..]),
        ) {
            if slot != 0 {
                return (set, Some(WriterScope::new(node, slot as u32)));
            }
        }
    }
    (generation, None)
}

/// How a staging root's marker binding relates to the generation this
/// mount wants (the [`crate::cache::nvme`] gate's decision input).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationBinding {
    /// Same set, same scope: keep everything (the warm-restart arm).
    Match,
    /// Same set, an UPGRADE-adoptable scope difference — three arms, all
    /// "this node's own root under an older/coarser identity form":
    ///
    /// * marker carries NO scope while we are scoped — the Phase-8
    ///   upgrade arm: the root was stamped before the bit was, its
    ///   content is ours under the single-writer guard that wrote it.
    ///   (Also the belt-and-braces arm for a KD-8 rebind performed by a
    ///   binary that had not resolved a scope.)
    /// * marker carries OUR node, node-only, while we are pair-scoped
    ///   (KD-MW-2) — a pre-pair binary's root at our path, or a root the
    ///   `staging adopt` verb re-bound to the node.
    /// * marker carries OUR node with a slot while WE are node-only —
    ///   the OFFLINE identity (KD-8 verbs, `staging adopt`): those verbs
    ///   must carry every one of this node's roots across a membership
    ///   change, or the next mount's dead-generation discard destroys
    ///   acked staged payloads. No MOUNT ever runs node-only, so this
    ///   arm is unreachable at the mount gate.
    ///
    /// In every arm: adopt and re-stamp with OUR generation, never
    /// discard.
    ScopeUpgrade,
    /// Same set, marker carries a scope we cannot claim: another NODE's
    /// root, another MOUNT SLOT of this node (a co-located sibling, or
    /// the moved-mount-point residue MW-1b reports), or the unprovable
    /// downgrade direction. NEVER wiped while it holds live custody —
    /// that would destroy another writer's acked staged payloads.
    ForeignScope(WriterScope),
    /// Different set: the dead-generation arm (a reformat, or a foreign
    /// filesystem's staging) — discard, exactly as before.
    ForeignSet,
}

/// Classify one marker-bound generation against `(our set generation, our
/// scope)`. The whole item-10 decision table lives here so the gate, the
/// KD-8 barrier and the MW-1b residue scan cannot drift.
pub fn classify_generation(
    marker: &str,
    set_generation: &str,
    scope: Option<WriterScope>,
) -> GenerationBinding {
    let (marker_set, marker_scope) = split_staging_generation(marker);
    let (want_set, _) = split_staging_generation(set_generation);
    if marker_set != want_set {
        return GenerationBinding::ForeignSet;
    }
    match (marker_scope, scope) {
        (None, None) => GenerationBinding::Match,
        (Some(m), Some(ours)) if m == ours => GenerationBinding::Match,
        (None, Some(_)) => GenerationBinding::ScopeUpgrade,
        // KD-MW-2's same-node upgrade arms (enum docs): a node-only
        // marker under our pair, or our node's slotted root under the
        // offline node-only identity.
        (Some(m), Some(ours)) if m.node == ours.node && (m.slot == 0 || ours.slot == 0) => {
            GenerationBinding::ScopeUpgrade
        }
        (Some(m), _) => GenerationBinding::ForeignScope(m),
    }
}

/// Engagement from ALREADY-OPEN superblock feature words — the
/// fsck/defrag/job path, which holds the volumes open and must not pay a
/// second sector-0 read to reach the same verdict as
/// [`resolve_scope_for_set`]. Same unanimity rule; a node-identity failure
/// degrades to UNSCOPED loudly (tooling must not wedge on it — the
/// consequence is at worst a marker comparison that reports the
/// `ScopeUpgrade` shape, which is not a finding).
///
/// Answers the NODE-ONLY scope: the fsck/job processes have no mount
/// point, and the node-only identity is exactly what makes this node's
/// pair-scoped roots rebindable-not-foreign to them (KD-MW-2, the
/// [`GenerationBinding::ScopeUpgrade`] offline arm).
pub fn scope_for_features(features: impl IntoIterator<Item = u64>) -> Option<WriterScope> {
    use crate::meta_backend::kv::superblock::FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING;
    let mut any = false;
    for f in features {
        any = true;
        if f & FEATURE_INCOMPAT_KV_WRITER_SCOPED_STAGING == 0 {
            return None;
        }
    }
    if !any {
        return None;
    }
    match resolve_node_identity() {
        Ok(id) => Some(WriterScope::node_only(id.token)),
        Err(e) => {
            log::warn!("writer-scoped set with no resolvable node identity ({e})");
            None
        }
    }
}

/// `true` ⇔ a staging root bound to `marker` is one THIS process may
/// rebind onto `staging_generation` (the KD-8 barrier's membership test,
/// design-volume-lifecycle KD-8 / §5.5.2).
///
/// Deliberately accepts [`GenerationBinding::ScopeUpgrade`] as well as
/// [`GenerationBinding::Match`]: a root stamped before the set's bit was
/// stamped carries the UN-scoped generation, and skipping it here would
/// leave it bound to the OLD set generation — the next mount would then
/// classify it `ForeignSet` and DISCARD durable acked staged payloads.
/// That is a data-loss path, pinned in
/// `tests/writer_scoped_staging_tests.rs`.
pub fn marker_is_rebindable(marker: &str, staging_generation: &str) -> bool {
    let (_, scope) = split_staging_generation(staging_generation);
    matches!(
        classify_generation(marker, staging_generation, scope),
        GenerationBinding::Match | GenerationBinding::ScopeUpgrade
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reboot argument, mechanized: the same source bytes always
    /// derive the same token (a file in `/etc` survives a reboot, so
    /// legitimate staged work is never orphaned), and distinct hosts
    /// derive distinct tokens.
    #[test]
    fn node_token_is_stable_and_distinct() {
        let a = derive_node_token(b"9f2c1d8e4b7a4c3f8e1d2c3b4a5f6e7d\n").unwrap();
        let a_again = derive_node_token(b"9f2c1d8e4b7a4c3f8e1d2c3b4a5f6e7d").unwrap();
        assert_eq!(a, a_again, "trailing whitespace must not change identity");
        let b = derive_node_token(b"0123456789abcdef0123456789abcdef").unwrap();
        assert_ne!(a, b, "different hosts derive different tokens");
        assert_ne!(a, 0, "0 is the reserved no-scope token");
    }

    /// Material that cannot identify a host is refused, never hashed
    /// into a confident-looking token.
    #[test]
    fn node_token_refuses_unusable_material() {
        assert!(derive_node_token(b"").is_none());
        assert!(derive_node_token(b"   \n\t ").is_none());
        assert!(derive_node_token(b"short").is_none(), "under 8 bytes");
        assert!(
            derive_node_token(b"00000000000000000000000000000000").is_none(),
            "systemd's uninitialized machine-id is identical everywhere"
        );
    }

    /// The raw identity material never appears in the token rendering.
    #[test]
    fn token_rendering_does_not_leak_the_machine_id() {
        let raw = "9f2c1d8e4b7a4c3f8e1d2c3b4a5f6e7d";
        let token = derive_node_token(raw.as_bytes()).unwrap();
        let rendered = format!("{token:016x}");
        assert!(!raw.contains(&rendered));
        assert_eq!(rendered.len(), 16);
    }

    /// KD-MW-2: the mount slot is mount-point-stable (same canonical path
    /// ⇒ same slot — the crash successor at the same path is the SAME
    /// client) and distinct across co-located paths, and never lands on
    /// the reserved node-only sentinel 0.
    #[test]
    fn mount_slot_is_path_stable_distinct_and_never_zero() {
        let a = derive_mount_slot("/mnt/train-a");
        assert_eq!(a, derive_mount_slot("/mnt/train-a"), "path-stable");
        assert_ne!(
            a,
            derive_mount_slot("/mnt/train-b"),
            "co-located mounts derive distinct slots"
        );
        assert_ne!(a, 0, "0 is the reserved node-only sentinel");
    }

    /// The `-o client_slot=<hex8>` value law: parse-or-refuse, never a
    /// silent default (a wrong slot strands or mis-adopts acked custody).
    #[test]
    fn client_slot_option_parses_or_refuses() {
        assert_eq!(client_slot_from_options(None).unwrap(), None);
        assert_eq!(
            client_slot_from_options(Some("allow_other,client_slot=00c0ffee")).unwrap(),
            Some(0x00c0_ffee)
        );
        assert_eq!(
            client_slot_from_options(Some("rw,noexec")).unwrap(),
            None,
            "absent option is absent"
        );
        for bad in [
            "client_slot=",
            "client_slot=0",
            "client_slot=xyz",
            "client_slot=123456789",
        ] {
            let err = client_slot_from_options(Some(bad)).unwrap_err().to_string();
            assert!(err.contains("client_slot"), "{bad}: {err}");
        }
    }

    /// The pair renders and round-trips through both grammars (key suffix
    /// and generation decoration), and the node-only spellings stay
    /// byte-identical to the pre-pair forms.
    #[test]
    fn pair_scope_round_trips_both_grammars() {
        let pair = WriterScope::new(0x0123_4567_89ab_cdef, 0x00c0_ffee);
        let node = WriterScope::node_only(0x0123_4567_89ab_cdef);
        assert_eq!(pair.render(), "w_0123456789abcdef.m00c0ffee");
        assert_eq!(node.render(), "w_0123456789abcdef");

        let key = format!("active_block:inode_7:block_3:{}", pair.render());
        assert_eq!(key_scope(&key), Some(pair));
        assert_eq!(strip_key_scope(&key), "active_block:inode_7:block_3");
        let key_node = format!("active_block:inode_7:block_3:{}", node.render());
        assert_eq!(key_scope(&key_node), Some(node));
        assert_eq!(strip_key_scope(&key_node), "active_block:inode_7:block_3");

        let set = "v3:aabb";
        let g = staging_generation(set, Some(pair));
        assert_eq!(g, "v3:aabb@node:0123456789abcdef.m00c0ffee");
        assert_eq!(split_staging_generation(&g), (set, Some(pair)));
        let gn = staging_generation(set, Some(node));
        assert_eq!(
            gn, "v3:aabb@node:0123456789abcdef",
            "the node-only decoration is byte-identical to the pre-pair form"
        );
        assert_eq!(split_staging_generation(&gn), (set, Some(node)));
        // A pair spelling of slot 0 is never minted and never parses.
        assert_eq!(
            split_staging_generation("v3:aabb@node:0123456789abcdef.m00000000").1,
            None
        );
    }
}
