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

use crate::error::{Result, SqueezefsError};

/// Domain-separation seed for the app-specific node-token derivation (the
/// raw machine-id is treated as confidential and never lands on disk).
const NODE_SCOPE_APP_SEED: u64 = u64::from_be_bytes(*b"SQZNODE1");

/// Reserved token: `0` means "no scope" everywhere in this module, so a
/// derivation that lands on 0 is nudged to 1.
const NO_SCOPE: u64 = 0;

/// Key component prefix: keys carry `…:w_{16 hex}` as their LAST
/// component (see [`scoped_key_suffix`]).
const KEY_SCOPE_TAG: &str = ":w_";

/// Rendered key-scope suffix length: `":w_"` + 16 hex.
const KEY_SCOPE_LEN: usize = KEY_SCOPE_TAG.len() + 16;

/// Staging-generation decoration: `{set_generation}@node:{16 hex}`.
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
/// `Ok(None)` = disengaged, i.e. EXACTLY today's behavior. `Ok(Some(t))`
/// = scoped with node token `t`. The superblock probe is the same 4 KiB
/// sector-0 read `volume_set_generation` performs, deliberately repeated
/// here rather than threaded through `MetaSetDiscovery` so this stays one
/// self-contained mount-path function.
pub async fn resolve_scope_for_set(meta_lvs: &[String]) -> Result<Option<u64>> {
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
    Ok(Some(resolve_node_identity()?.token))
}

// ---------------------------------------------------------------------------
// Process-global engagement (mount-time, once)
// ---------------------------------------------------------------------------

/// The process's scope token; `0` = disengaged (a derived token is never
/// 0 — see [`derive_node_token`]).
///
/// ONE relaxed atomic word, deliberately: key minting is on the
/// small-write hot path, where the house law is latch-free. A `RwLock`
/// here measured 11× on the mount-path classification sweep and would
/// serialize every writer's key mint on one cache line
/// (`benches/write_path_bench.rs` `writer_scope` group).
static SCOPE_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(NO_SCOPE);

/// Engage (or, with `None`, disengage) the process-global writer scope.
/// Called ONCE per process from the mount path after
/// [`resolve_scope_for_set`]; the suites call it directly to drive both
/// arms without touching a superblock.
pub fn engage(scope: Option<u64>) {
    SCOPE_TOKEN.store(
        scope.unwrap_or(NO_SCOPE),
        std::sync::atomic::Ordering::Release,
    );
}

/// `true` ⇔ this process mints writer-scoped staging keys.
#[inline]
pub fn engaged() -> bool {
    SCOPE_TOKEN.load(std::sync::atomic::Ordering::Relaxed) != NO_SCOPE
}

/// This process's scope token, or `None` when disengaged.
#[inline]
pub fn engaged_scope() -> Option<u64> {
    match SCOPE_TOKEN.load(std::sync::atomic::Ordering::Relaxed) {
        NO_SCOPE => None,
        token => Some(token),
    }
}

/// A rendered `":w_{16 hex}"` key component — fixed width, built on the
/// stack (no global string, no allocation, no lock).
#[derive(Clone, Copy)]
pub struct ScopeSuffix {
    buf: [u8; KEY_SCOPE_LEN],
}

impl ScopeSuffix {
    #[inline]
    fn render(token: u64) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut buf = [0u8; KEY_SCOPE_LEN];
        buf[..KEY_SCOPE_TAG.len()].copy_from_slice(KEY_SCOPE_TAG.as_bytes());
        for i in 0..16 {
            let nibble = (token >> (60 - 4 * i)) & 0xF;
            buf[KEY_SCOPE_TAG.len() + i] = HEX[nibble as usize];
        }
        Self { buf }
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        // Only ASCII was written (the tag plus lowercase hex).
        std::str::from_utf8(&self.buf).expect("scope suffix is ASCII")
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
    match SCOPE_TOKEN.load(std::sync::atomic::Ordering::Relaxed) {
        NO_SCOPE => None,
        token => Some(ScopeSuffix::render(token)),
    }
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
    /// Scope component equals ours.
    Mine,
    /// Scope component names another node. Never adopted, never flushed,
    /// never freed: the payload lives in THAT node's staging ring, so
    /// there is nothing here we could recover even if custody allowed it.
    Foreign(u64),
    /// Scoped record while WE are unscoped: unprovable ownership. Treated
    /// exactly like [`KeyOwner::Foreign`] (never adopted) — the
    /// declared-unsupported downgrade direction, forward-detected.
    Unprovable(u64),
}

impl KeyOwner {
    /// `true` ⇔ this process may recover, fold, flush and free the record.
    #[inline]
    pub fn is_mine(&self) -> bool {
        matches!(self, KeyOwner::Legacy | KeyOwner::Mine)
    }
}

/// The scope token a key carries, if any: the trailing `:w_{16 lowercase
/// hex}` component. Byte-wise + ASCII-only, so it is UTF-8-boundary safe
/// on arbitrary keys and cheap enough for the recovery scan.
///
/// No unscoped key of any shipped release can match: every unscoped form
/// ends in `block_{digits}` or a uuid `file_id`.
#[inline]
pub fn key_scope(key: &str) -> Option<u64> {
    let b = key.as_bytes();
    if b.len() < KEY_SCOPE_LEN {
        return None;
    }
    let tail = &b[b.len() - KEY_SCOPE_LEN..];
    if &tail[..KEY_SCOPE_TAG.len()] != KEY_SCOPE_TAG.as_bytes() {
        return None;
    }
    let mut token: u64 = 0;
    for c in &tail[KEY_SCOPE_TAG.len()..] {
        let nibble = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            // Uppercase is deliberately NOT accepted: we mint lowercase,
            // so accepting both would make two spellings of one scope.
            _ => return None,
        };
        token = (token << 4) | nibble as u64;
    }
    Some(token)
}

/// A key with its scope component removed — what the historical
/// `strip_prefix(…)` / `split_once(":block_")` parsers must see.
#[inline]
pub fn strip_key_scope(key: &str) -> &str {
    if key_scope(key).is_some() {
        &key[..key.len() - KEY_SCOPE_LEN]
    } else {
        key
    }
}

/// Classify a staging key against this process's scope.
#[inline]
pub fn classify_key(key: &str) -> KeyOwner {
    match (key_scope(key), engaged_scope()) {
        (None, _) => KeyOwner::Legacy,
        (Some(k), Some(mine)) if k == mine => KeyOwner::Mine,
        (Some(k), Some(_)) => KeyOwner::Foreign(k),
        (Some(k), None) => KeyOwner::Unprovable(k),
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
pub fn staging_generation(set_generation: &str, scope: Option<u64>) -> String {
    match scope {
        Some(token) => format!("{set_generation}{GENERATION_SCOPE_TAG}{token:016x}"),
        None => set_generation.to_string(),
    }
}

/// Split a staging generation into `(set generation, node scope)`. An
/// un-decorated string reports the whole value and `None` (so every
/// pre-change marker parses as "this set, no node").
pub fn split_staging_generation(generation: &str) -> (&str, Option<u64>) {
    let Some(idx) = generation.rfind(GENERATION_SCOPE_TAG) else {
        return (generation, None);
    };
    let (set, tail) = generation.split_at(idx);
    let hex = &tail[GENERATION_SCOPE_TAG.len()..];
    if hex.len() != 16
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return (generation, None);
    }
    match u64::from_str_radix(hex, 16) {
        Ok(token) => (set, Some(token)),
        Err(_) => (generation, None),
    }
}

/// How a staging root's marker binding relates to the generation this
/// mount wants (the [`crate::cache::nvme`] gate's decision input).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationBinding {
    /// Same set, same scope: keep everything (the warm-restart arm).
    Match,
    /// Same set, marker carries NO scope while we are scoped: the
    /// Phase-8 UPGRADE arm — the root was stamped before the bit was,
    /// its content is ours under the single-writer guard that wrote it,
    /// so adopt and re-stamp scoped. (Also the belt-and-braces arm for a
    /// KD-8 rebind performed by a binary that had not resolved a scope.)
    ScopeUpgrade,
    /// Same set, marker carries a scope we cannot claim: a peer node's
    /// root (or the unprovable downgrade direction). NEVER wiped while it
    /// holds live custody — that would destroy another writer's acked
    /// staged payloads.
    ForeignScope(u64),
    /// Different set: the dead-generation arm (a reformat, or a foreign
    /// filesystem's staging) — discard, exactly as before.
    ForeignSet,
}

/// Classify one marker-bound generation against `(our set generation, our
/// scope)`. The whole item-10 decision table lives here so the gate and
/// the KD-8 barrier cannot drift.
pub fn classify_generation(
    marker: &str,
    set_generation: &str,
    scope: Option<u64>,
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
pub fn scope_for_features(features: impl IntoIterator<Item = u64>) -> Option<u64> {
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
        Ok(id) => Some(id.token),
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
}
