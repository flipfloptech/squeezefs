//! Symmetric metadata program, PR 4 — **slot leases**
//! (`docs/design-symmetric-metadata.md` §5.1 acquisition / minting /
//! hold / handover / resolution, §5.3.4 rows 5–8, §5.3.5 idempotent verbs,
//! §5.4.1 the partition law, §8 gate 1's engagement laws, §11 the
//! Slot-lease family; KD-SYM-1/2/4/5/11/16/17).
//!
//! Under `SQUEEZEFS_SYMMETRIC_META=1` on a bit-17 volume the writer that
//! wins the D0 ladder leases its native slot and `M` rotor slots through
//! the manager's `AcquireSlots`, mints by the bounded parent-slot affinity
//! policy, refuses a leaf mutation of a slot it does not lease, homes the
//! DLM on the live lease map (`dlm_mode` = `slot-homed`) and hands a slot
//! over — flush-then-transfer — to ONE requester whose ships dominate its
//! own ops over a common window. The multi-appender shape here is PR 2/3's:
//! the manager's backend holds every LIVE region (region 0 + the declared
//! seam's), wire joiners are identities with pages (`ManagerClient`), and
//! ships are counted as the design says they are ("the count is what
//! matters"); N daemon processes on one volume is PR 12's join ladder.
//!
//! **`SQUEEZEFS_SYMMETRIC_META=0` is the shipped posture exactly**: the
//! PR 1–3 forest — every slot the mount's, the shared mint rotor,
//! `dlm_mode` = `solo` — and a bit-17-absent volume is untouched.

use squeezefs::meta_backend::kv::appender::{
    read_directory, AppenderIdentity, AppenderState, SlotEntryState, TEST_APPENDER_SLOTS_ENV,
};
use squeezefs::meta_backend::kv::backend::{
    AcquireSlotReply, KvMetaBackend, TEST_HANDOVER_HOLD_AFTER_PAGE, TEST_HANDOVER_HOLD_AFTER_TREE0,
};
use squeezefs::meta_backend::kv::block_refs::{volume_tag, BlockRef, BlockRefOp};
use squeezefs::meta_backend::kv::builder::{
    digest_backend, format_v3_stamped, FormatV3Options, ROOT_INO,
};
use squeezefs::meta_backend::kv::record::{
    forest_slot_of_ino, guest_forest_slot, ForestSlot, NATIVE_FOREST_SLOT,
};
use squeezefs::meta_backend::kv::slot_lease::{
    resolve_affinity_ceiling, resolve_mint_slots, resolve_t_idle_ms, SlotLeaseStats,
    SYMMETRIC_META_ENV, SYM_AFFINITY_MAX_MB_ENV, SYM_MINT_SLOTS_ENV, SYM_T_IDLE_MS_ENV,
};
use squeezefs::meta_backend::kv::slot_state::{
    decode_slot_state_key, slot_state_key_range, SlotState,
};
use squeezefs::meta_backend::kv::{
    META_KV_LEAF_LEASE_REFUSALS, META_KV_REPLAY_KEY_VIOLATIONS, META_KV_REPLAY_LEASE_VIOLATIONS,
};
use squeezefs::meta_backend::{
    open_routed_meta_set, plan_meta_slot_set, Metadata, RoutedMetaBackend, MINT_SPREAD,
};
use squeezefs::slot_lease_core::{ShipVerdict, MINT_SPREAD as CORE_SPREAD};
use std::sync::atomic::Ordering;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Harness (the sym_manager_tests shape: 64 KiB nodes, a 1 MiB fixed ring).
// ---------------------------------------------------------------------------

const VOL_LEN: u64 = 64 * 1024 * 1024;
const NODE_SIZE: usize = 64 * 1024;
const RING_LEN: u64 = 1024 * 1024;
/// The declared seam: appender 1 leases forest slot 4 (routing slot 3).
const PARTITION: &str = "1:4";
const SLOT4: ForestSlot = 4;
/// The seam's ALTERNATE: appender 1 declares forest slot 200 (routing
/// 199) — outside the manager's rotor (forest slots 2..=65), so a remount
/// under it leaves slot 4 to whatever tree 0 says.
const PARTITION_ALT: &str = "1:200";
const SLOT_ALT: ForestSlot = 200;

/// The seams and knobs are process-global; every test serializes on it.
static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

async fn format_stamped_member(dir: &std::path::Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format stamped member");
    p.display().to_string()
}

async fn format_flat_member(dir: &std::path::Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone())
        .await
        .expect("format flat member");
    p.display().to_string()
}

/// The knobs a mount reads at open, restored after it.
struct Knobs {
    armed: bool,
    partition: Option<&'static str>,
    mint_slots: Option<&'static str>,
    affinity_mb: Option<&'static str>,
    t_idle_ms: Option<&'static str>,
}

impl Knobs {
    fn armed() -> Self {
        Self {
            armed: true,
            partition: None,
            mint_slots: None,
            affinity_mb: None,
            t_idle_ms: None,
        }
    }
    fn unarmed() -> Self {
        Self {
            armed: false,
            ..Self::armed()
        }
    }
    fn partition(mut self, p: &'static str) -> Self {
        self.partition = Some(p);
        self
    }
    fn mint_slots(mut self, m: &'static str) -> Self {
        self.mint_slots = Some(m);
        self
    }
    fn affinity_mb(mut self, mb: &'static str) -> Self {
        self.affinity_mb = Some(mb);
        self
    }
    fn t_idle_ms(mut self, ms: &'static str) -> Self {
        self.t_idle_ms = Some(ms);
        self
    }
    fn apply(&self) {
        if self.armed {
            std::env::set_var(SYMMETRIC_META_ENV, "1");
        } else {
            std::env::remove_var(SYMMETRIC_META_ENV);
        }
        std::env::set_var("SQUEEZEFS_SYM_ALLOW_NON_PR", "1");
        match self.partition {
            Some(p) => std::env::set_var(TEST_APPENDER_SLOTS_ENV, p),
            None => std::env::remove_var(TEST_APPENDER_SLOTS_ENV),
        }
        match self.mint_slots {
            Some(m) => std::env::set_var(SYM_MINT_SLOTS_ENV, m),
            None => std::env::remove_var(SYM_MINT_SLOTS_ENV),
        }
        match self.affinity_mb {
            Some(mb) => std::env::set_var(SYM_AFFINITY_MAX_MB_ENV, mb),
            None => std::env::remove_var(SYM_AFFINITY_MAX_MB_ENV),
        }
        match self.t_idle_ms {
            Some(ms) => std::env::set_var(SYM_T_IDLE_MS_ENV, ms),
            None => std::env::remove_var(SYM_T_IDLE_MS_ENV),
        }
    }
    fn clear() {
        for k in [
            SYMMETRIC_META_ENV,
            "SQUEEZEFS_SYM_ALLOW_NON_PR",
            TEST_APPENDER_SLOTS_ENV,
            SYM_MINT_SLOTS_ENV,
            SYM_AFFINITY_MAX_MB_ENV,
            SYM_T_IDLE_MS_ENV,
        ] {
            std::env::remove_var(k);
        }
    }
}

/// Open the set under `knobs`; the knobs are cleared after the open (a
/// mount reads them once, at open — the plane keeps them).
async fn open_under(uris: &[String], knobs: &Knobs) -> Arc<RoutedMetaBackend> {
    knobs.apply();
    let r = open_routed_meta_set(uris).await;
    Knobs::clear();
    r.expect("open routed set")
}

fn lease_stats(vol: &KvMetaBackend) -> SlotLeaseStats {
    vol.slot_lease_stats().expect("an armed volume has a plane")
}

/// Tree 0's `slot_state` population: `(forest slot, state)`.
async fn tree0_states(vol: &KvMetaBackend) -> Vec<(ForestSlot, SlotState)> {
    let control = vol.forest_control_tree().expect("a forest has tree 0");
    let (mut cursor, end) = slot_state_key_range();
    let mut out = Vec::new();
    loop {
        let page = control.range(&cursor, &end, 512).await.unwrap();
        let Some((last, _)) = page.last() else {
            break;
        };
        cursor = squeezefs::meta_backend::kv::node::key_successor(last);
        for (k, v) in &page {
            out.push((
                decode_slot_state_key(k).unwrap(),
                SlotState::decode(v).unwrap(),
            ));
        }
        if page.len() < 512 {
            break;
        }
    }
    out
}

fn refs(tag: u64, owner: u64, base: u64, n: u64) -> Vec<BlockRefOp> {
    (0..n)
        .map(|i| {
            BlockRefOp::taken(BlockRef {
                vol_tag: tag,
                block_idx: base + i,
                owner_ino: owner,
                block_index: i as u32,
            })
        })
        .collect()
}

/// An ino inside forest slot `slot`'s guest keyspace (local key ino).
fn ino_in_slot(slot: ForestSlot, local: u64) -> u64 {
    squeezefs::meta_backend::guest_local_ino((slot - 1) as u16, local)
}

async fn shutdown(routed: &RoutedMetaBackend) {
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// §5.1.2 / §8 gate 1 — the solo armed mount, and the shipped posture.
// ---------------------------------------------------------------------------

/// Gate 1's engagement law: a solo armed mount leases its native slot
/// plus 64 rotor slots (`M = clamp(W/(2×1), 1, 64)`), every one recorded
/// `Leased { 0, g = 1 }` in tree 0 and on page 0 with `g = 1`, the DLM
/// homed on the lease map (`slot-homed`, `dlm_rpcs == 0`), the overflow
/// gauge 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_solo_armed_mount_leases_its_native_slot_and_sixty_four_rotor_slots() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert!(vol.slot_lease_armed());
    let s = lease_stats(&vol);
    assert_eq!(s.rotor, MINT_SPREAD as u64);
    assert_eq!(CORE_SPREAD, MINT_SPREAD as u64, "the core's spread is the routed layer's");
    assert_eq!(
        s.leases_held,
        1 + MINT_SPREAD as u64,
        "1 native + 64 rotor: {s:?}"
    );
    assert_eq!(s.ceiling_overflows, 0, "a solo mount never asks for a 65th");
    assert_eq!(s.grants, 1 + MINT_SPREAD as u64);
    assert_eq!(s.conflicts, 0);
    assert_eq!(squeezefs::dlm_slot::dlm_mode(), "slot-homed");
    assert_eq!(squeezefs::dlm_slot::dlm_rpcs(), 0, "solo: nothing is foreign");
    // Tree 0: 65 Leased{0, g = 1} records; the native slot among them.
    let states = tree0_states(&vol).await;
    let leased: Vec<&(ForestSlot, SlotState)> = states
        .iter()
        .filter(|(_, st)| matches!(st, SlotState::Leased { .. }))
        .collect();
    assert_eq!(leased.len(), 1 + MINT_SPREAD, "{states:?}");
    for (slot, st) in &leased {
        let SlotState::Leased { appender_id, g, .. } = st else {
            unreachable!()
        };
        assert_eq!((*appender_id, *g), (0, 1), "slot {slot}: {st:?}");
    }
    assert!(leased.iter().any(|(s, _)| *s == NATIVE_FOREST_SLOT));
    // The rotor is the first 64 non-native slots (never-written first,
    // ties by index).
    for r in 1..=64u16 {
        assert!(
            leased.iter().any(|(s, _)| *s == guest_forest_slot(r)),
            "routing slot {r} is in the rotor"
        );
    }
    // A checkpoint writes page 0 with every leased slot at g = 1.
    vol.checkpoint_now().await.unwrap();
    let entries = read_directory(std::path::Path::new(&uris[0]), vol.superblock())
        .await
        .unwrap();
    let page0 = entries[0].page.clone().expect("page 0");
    assert_eq!(page0.state, AppenderState::Live);
    assert_eq!(page0.slots.len(), 1 + MINT_SPREAD, "the page names exactly the leases");
    assert!(page0.slots.iter().all(|e| e.g == 1));
    assert!(page0
        .slots
        .iter()
        .all(|e| e.state == SlotEntryState::Live));
    shutdown(&routed).await;
    // The clean leave released every lease: tree 0 says Unleased at g = 1
    // with the release seq, for the next joiner's `unleased-then-idle`.
    let routed = open_under(&uris, &Knobs::unarmed()).await;
    let states = tree0_states(&routed.volumes[0]).await;
    assert!(
        states
            .iter()
            .all(|(_, st)| matches!(st, SlotState::Unleased { g: 1, .. })),
        "{states:?}"
    );
    assert!(states
        .iter()
        .any(|(_, st)| matches!(st, SlotState::Unleased { last_written, .. } if *last_written > 0)));
    shutdown(&routed).await;
}

/// `SQUEEZEFS_SYMMETRIC_META=0` is the PR 1–3 forest exactly: no plane,
/// `dlm_mode` = `solo`, tree 0 holds only the checkpoint's `Unleased`
/// roots at `g = 0`, page 0's entries carry `g = 0`, and the Slot-lease
/// family is absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symmetric_meta_off_is_the_shipped_dark_forest() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::unarmed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let refusals_before = META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed);
    assert!(!vol.slot_lease_armed());
    assert!(vol.slot_lease_stats().is_none());
    assert_eq!(squeezefs::dlm_slot::dlm_mode(), "solo");
    // A create mints through the shared rotor and publishes Unleased roots.
    for i in 0..8 {
        routed
            .create(ROOT_INO, &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
    }
    vol.checkpoint_now().await.unwrap();
    let states = tree0_states(&vol).await;
    assert!(!states.is_empty());
    assert!(
        states
            .iter()
            .all(|(_, st)| matches!(st, SlotState::Unleased { g: 0, .. })),
        "{states:?}"
    );
    let entries = read_directory(std::path::Path::new(&uris[0]), vol.superblock())
        .await
        .unwrap();
    let page0 = entries[0].page.clone().expect("page 0");
    assert!(page0.slots.iter().all(|e| e.g == 0 && e.slot_tree_extents == 0));
    assert_eq!(
        META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed),
        refusals_before,
        "the gate is inert unarmed"
    );
    shutdown(&routed).await;
}

/// `SQUEEZEFS_SYMMETRIC_META=1` on a bit-17-ABSENT volume refuses the
/// writer's open loud, naming `enable-symmetric`; the flat volume is
/// untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symmetric_meta_on_a_flat_volume_refuses_naming_enable_symmetric() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_flat_member(dir.path(), "meta0").await];
    Knobs::armed().apply();
    let r = open_routed_meta_set(&uris).await;
    Knobs::clear();
    let e = r.err().expect("refused").to_string();
    assert!(e.contains("enable-symmetric"), "{e}");
    assert!(e.contains("bit 17"), "{e}");
    // Unarmed, the flat volume mounts as ever.
    let routed = open_under(&uris, &Knobs::unarmed()).await;
    assert!(routed.volumes[0].appender_stats().is_none());
    assert!(routed.volumes[0].slot_lease_stats().is_none());
    shutdown(&routed).await;
}

/// The four knobs' derivations and ranges (the derivation law's tie): `M
/// = clamp(W/(2×writers), 1, 64)`, `A_max = max(used/64, node_size)` with
/// the static knob clamped to `[node_size, heap]`, `T_idle = T_owner`.
#[test]
fn the_knobs_derive_and_win_verbatim() {
    let w = u64::from(squeezefs::meta_backend::DERIVED_ROUTING_WIDTH);
    std::env::remove_var(SYM_MINT_SLOTS_ENV);
    assert_eq!(resolve_mint_slots(w, 1), 64);
    assert_eq!(resolve_mint_slots(w, 12_500), 2);
    std::env::set_var(SYM_MINT_SLOTS_ENV, "3");
    assert_eq!(resolve_mint_slots(w, 1), 3);
    std::env::remove_var(SYM_MINT_SLOTS_ENV);
    std::env::remove_var(SYM_AFFINITY_MAX_MB_ENV);
    assert_eq!(resolve_affinity_ceiling(0, 65_536, 1 << 30), 65_536);
    assert_eq!(resolve_affinity_ceiling(640 << 20, 65_536, 1 << 30), 10 << 20);
    std::env::set_var(SYM_AFFINITY_MAX_MB_ENV, "1");
    assert_eq!(resolve_affinity_ceiling(640 << 20, 65_536, 1 << 30), 1 << 20);
    std::env::remove_var(SYM_AFFINITY_MAX_MB_ENV);
    std::env::remove_var(SYM_T_IDLE_MS_ENV);
    std::env::remove_var("SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS");
    assert_eq!(
        resolve_t_idle_ms(),
        squeezefs::fuse_client::CLIENT_STALE_TTL_SECS * 1000
    );
    std::env::set_var(SYM_T_IDLE_MS_ENV, "2000");
    assert_eq!(resolve_t_idle_ms(), 2000);
    std::env::remove_var(SYM_T_IDLE_MS_ENV);
    for k in [
        SYMMETRIC_META_ENV,
        SYM_MINT_SLOTS_ENV,
        SYM_AFFINITY_MAX_MB_ENV,
        SYM_T_IDLE_MS_ENV,
    ] {
        assert!(squeezefs::env_knobs::lookup(k).is_some(), "{k} registered");
    }
    match squeezefs::env_knobs::lookup(SYM_MINT_SLOTS_ENV).unwrap().kind {
        squeezefs::env_knobs::Kind::Int { lo, hi } => assert_eq!((lo, hi), (1, 64)),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// §5.1.2 — the mint policy: divisibility on a solo mount, the spill, the
// strictly-over-cap overflow.
// ---------------------------------------------------------------------------

/// The forest slot the inode `ino` (global) lives in on a one-volume set.
fn slot_of_global(routed: &RoutedMetaBackend, ino: u64) -> ForestSlot {
    let (_, local) = routed.route_ino(ino);
    forest_slot_of_ino(local)
}

/// Gate 1's divisibility law as a LOCAL contract: a solo mount's load
/// spreads over the 64 rotor trees whatever its top-level shape (mdstorm's
/// two directories), no tree past `A_max(end) + node_size`, no overflow
/// request, every mint accounted (`affinity_mints + rotor_mints ≡ mints`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_solo_mounts_load_spreads_over_sixty_four_rotor_trees_whatever_its_top_level_shape() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let storm = routed
        .create(ROOT_INO, "storm", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let manydirs = routed
        .create(ROOT_INO, "manydirs", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    // Two top-level shapes: one flat directory of files, one of
    // directories with a file each.
    // The cadence tick (the background task's maintenance pass runs the
    // splits the cap's ledger reads) is driven here every 100 creates —
    // the design's "≤ 1 s stale" soft cap at this harness's rate.
    const FILES: usize = 1_500;
    for i in 0..FILES {
        routed
            .create(storm.ino, &format!("f{i:05}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        if i % 100 == 99 {
            vol.checkpoint_now().await.unwrap();
        }
    }
    for i in 0..FILES / 2 {
        let d = routed
            .create(manydirs.ino, &format!("d{i:05}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap();
        routed
            .create(d.ino, "x", libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        if i % 50 == 49 {
            vol.checkpoint_now().await.unwrap();
        }
    }
    vol.checkpoint_now().await.unwrap();
    let s = lease_stats(&vol);
    {
        let plane = vol.slot_leases().expect("armed");
        let rotor = plane.rotor.load_full();
        let mut sizes: Vec<u64> = rotor.iter().map(|s| plane.extents.get(*s)).collect();
        sizes.sort_unstable();
        eprintln!(
            "solo spread row (dev box — scoping): max tree {} B, A_max {} B, affinity {} / rotor \
             {} mints, spills {}; extents per rotor tree {sizes:?}",
            s.tree_bytes_max, s.a_max_bytes, s.affinity_mints, s.rotor_mints, s.ceiling_spills
        );
    }
    let mints = 2 + FILES as u64 + FILES as u64;
    assert_eq!(
        s.affinity_mints + s.rotor_mints,
        mints,
        "affinity_mints + rotor_mints ≡ mints: {s:?}"
    );
    assert_eq!(s.ceiling_overflows, 0, "a solo mount never asks for a 65th");
    assert_eq!(s.leases_held, 1 + MINT_SPREAD as u64);
    let node_size = NODE_SIZE as u64;
    // Gate 1's law is `max ≤ A_max(end) + node_size` on LEAF bytes at
    // mdstorm's scale (the box row). At THIS scale two discrete terms
    // stand beside the cap: the ledger counts every image of the tree
    // and a one-leaf tree over the cap splits into a root and two leaves
    // in ONE SMO (+2 extents), and the cap is SOFT — it reads the split
    // only when the maintenance tick runs it (the design's ≤ 1 s
    // staleness), so one tick's worth of records lands before the spill
    // engages: `ceil(creates per tick × 3 records × ~128 B / node_size)`
    // extents, one here. The local form carries exactly those terms;
    // the note's §6 names the slack the design's row understates.
    let lag_extents = (100u64 * 3 * 128).div_ceil(node_size);
    assert!(
        s.tree_bytes_max <= s.a_max_bytes + (2 + lag_extents) * node_size,
        "no tree over its share plus a split's and one tick's slack: max {} vs A_max {} + \
         {} × {node_size}",
        s.tree_bytes_max,
        s.a_max_bytes,
        2 + lag_extents
    );
    // Rotor trees with records ≥ min(64, ceil(total_leaf_bytes / A_max)).
    let plane = vol.slot_leases().expect("armed");
    let rotor = plane.rotor.load_full();
    let with_records = rotor
        .iter()
        .filter(|s| plane.extents.get(**s) > 0)
        .count() as u64;
    let total_leaf_bytes: u64 = rotor
        .iter()
        .map(|s| plane.extents.get(*s) * node_size)
        .sum();
    let want = (total_leaf_bytes.div_ceil(s.a_max_bytes.max(1))).min(MINT_SPREAD as u64);
    assert!(
        with_records >= want,
        "rotor trees with records {with_records} ≥ min(64, ceil({total_leaf_bytes} / {})) = {want}",
        s.a_max_bytes
    );
    // Every inode of the two shapes lives in a LEASED non-native slot.
    for name in ["storm", "manydirs"] {
        let d = routed.lookup(ROOT_INO, name).await.unwrap();
        let slot = slot_of_global(&routed, d.ino);
        assert_ne!(slot, NATIVE_FOREST_SLOT, "{name} is not in the native slot");
        assert!(plane.gate.is_leased(slot));
    }
    eprintln!("solo spread row: {with_records} rotor trees with records (want ≥ {want})");
    shutdown(&routed).await;
}

/// A tree past `A_max(t)` spills its NEW children to the rotor slot with
/// the most headroom (`affinity_ceiling_spills`): under the STATIC
/// ceiling `SQUEEZEFS_SYM_AFFINITY_MAX_MB=1`, a directory whose tree is
/// seeded past 1 MiB stops receiving its children.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tree_past_a_max_spills_new_children_to_the_rotor_slot_with_the_most_headroom() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed().affinity_mb("1")).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    let job = routed
        .create(ROOT_INO, "job", libc::S_IFDIR | 0o755, 0, 0)
        .await
        .unwrap();
    let job_slot = slot_of_global(&routed, job.ino);
    // Below the cap: children follow their parent (affinity).
    let a = routed
        .create(job.ino, "a", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    assert_eq!(slot_of_global(&routed, a.ino), job_slot, "affinity below the cap");
    let before = lease_stats(&vol);
    assert!(before.affinity_mints >= 1);
    // Seed the job tree's durable size past the 1 MiB static ceiling (16
    // extents of 64 KiB + 1): the cap is soft and its input is the
    // ledger, so the decision reads exactly what a grown tree would.
    plane.extents.set(job_slot, 17);
    let b = routed
        .create(job.ino, "b", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    let b_slot = slot_of_global(&routed, b.ino);
    assert_ne!(b_slot, job_slot, "past the cap the child spills to the rotor");
    assert!(plane.gate.is_leased(b_slot));
    let after = lease_stats(&vol);
    assert_eq!(after.ceiling_spills, before.ceiling_spills + 1);
    assert_eq!(after.a_max_bytes, 1 << 20, "the static ceiling in force");
    // The spill target is the rotor slot with the MOST headroom: no rotor
    // tree is larger... every other rotor tree is ≤ the picked one's
    // headroom complement.
    let rotor = plane.rotor.load_full();
    let picked = plane.extents.get(b_slot);
    assert!(
        rotor.iter().all(|s| plane.extents.get(*s) >= picked - 1),
        "the most-headroom pick"
    );
    shutdown(&routed).await;
}

/// All rotors STRICTLY over the cap by ≥ 1 extent grant one more rotor
/// slot up to `2 × M` (`affinity_ceiling_overflows`); exact equality
/// with the cap never does; past `2 × M` the smallest rotor tree takes the
/// mint. `M = 2` here (`SQUEEZEFS_SYM_MINT_SLOTS`), the cap static 1 MiB.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_rotors_strictly_over_cap_grant_one_more_up_to_two_m_while_exact_equality_never_does() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed().mint_slots("2").affinity_mb("1")).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    assert_eq!(lease_stats(&vol).rotor, 2);
    assert_eq!(plane.rotor.load().len(), 2);
    let cap_extents = (1u64 << 20) / NODE_SIZE as u64; // 16
    // Exact equality: every rotor tree AT the cap — no overflow.
    for s in plane.rotor.load_full().iter() {
        plane.extents.set(*s, cap_extents);
    }
    routed
        .create(ROOT_INO, "eq", libc::S_IFREG | 0o644, 0, 0)
        .await
        .unwrap();
    assert_eq!(lease_stats(&vol).ceiling_overflows, 0, "exact equality never overflows");
    assert_eq!(plane.rotor.load().len(), 2);
    // Strictly over by one extent: the mint asks for one more (3rd), then
    // a 4th (= 2 × M); past it the smallest rotor tree.
    for round in 0..3 {
        for s in plane.rotor.load_full().iter() {
            plane.extents.set(*s, cap_extents + 1);
        }
        let f = routed
            .create(ROOT_INO, &format!("over{round}"), libc::S_IFREG | 0o644, 0, 0)
            .await
            .unwrap();
        let slot = slot_of_global(&routed, f.ino);
        assert!(plane.gate.is_leased(slot));
        let s = lease_stats(&vol);
        match round {
            0 => {
                assert_eq!(s.ceiling_overflows, 1);
                assert_eq!(plane.rotor.load().len(), 3);
                assert_eq!(*plane.rotor.load().last().unwrap(), slot, "minted in the new slot");
            }
            1 => {
                assert_eq!(s.ceiling_overflows, 2);
                assert_eq!(plane.rotor.load().len(), 4, "2 × M reached");
            }
            _ => {
                assert_eq!(s.ceiling_overflows, 2, "past 2 × M the manager refuses");
                assert_eq!(plane.rotor.load().len(), 4);
                let min_extents = plane
                    .rotor
                    .load()
                    .iter()
                    .map(|s| plane.extents.get(*s))
                    .min()
                    .unwrap();
                assert!(plane.rotor.load().contains(&slot), "a rotor slot");
                // The mint landed in the smallest tree (before its own
                // record moved the count): every rotor tree is at least
                // as large as the picked one was.
                assert!(
                    plane.extents.get(slot) <= min_extents + 1,
                    "the smallest rotor tree takes the mint"
                );
            }
        }
    }
    assert_eq!(lease_stats(&vol).leases_held, 1 + 4);
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// §5.1.2 / §5.4.1 — first-writer-takes-it, the foreign refusal, the gate.
// ---------------------------------------------------------------------------

use squeezefs::cluster_wire as cw;
use squeezefs::meta_ship::manager::{ManagerClient, ManagerReply, ManagerService};

const SECRET: &[u8] = b"sym-slot-transfer-tests-enroll-secret";

fn listener_cfg() -> cw::RpcListenerConfig {
    cw::RpcListenerConfig {
        bind_addr: "127.0.0.1:0".parse().expect("literal addr"),
        service_threads: 2,
        ..cw::RpcListenerConfig::default()
    }
}

fn joiner_identity(n: u64) -> AppenderIdentity {
    AppenderIdentity {
        node_token: 0x5EED_0000_0000_0000 | n,
        mount_slot: 0x1000 + n as u32,
        writer_id: 0xABCD_0000 + u128::from(n),
    }
}

/// A wire joiner: joined through `JoinAppender`, answering its appender
/// id and a client to keep issuing verbs with.
async fn wire_joiner(
    vol: &Arc<KvMetaBackend>,
    n: u64,
) -> (Arc<cw::RpcListener>, ManagerClient, u32) {
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        ManagerService::new(Arc::clone(vol)),
    )
    .expect("manager listener");
    let endpoint = host.endpoint().to_string();
    let mut client = ManagerClient::connect(&endpoint, SECRET, &format!("joiner-{n}"))
        .await
        .expect("storage-trust enrollment");
    let reply = client.join(joiner_identity(n), 0).await.unwrap();
    let ManagerReply::Joined { appender_id, .. } = reply else {
        panic!("{reply:?}");
    };
    (host, client, appender_id)
}

/// First-writer-takes-it (§5.1.2): a wire joiner's `AcquireSlot` on an
/// unleased slot is granted at `g = 1` and recorded `Leased { joiner }`
/// in tree 0 and on the joiner's page; the manager's own ask for it is
/// REFUSED naming the holder; the joiner's replay answers `already`
/// (KD-SYM-7); `ResolveSlot` names the holder; `dlm_mode` = `slot-homed`
/// with that one slot foreign.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_writer_takes_it_and_the_second_is_refused_naming_the_holder() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let (host, mut client, joiner) = wire_joiner(&vol, 1).await;
    // Routing slot 100 = forest slot 101: unleased on a fresh solo mount
    // (the rotor took 1..=64).
    let routing: u16 = 100;
    let fslot = guest_forest_slot(routing);
    let reply = client.acquire_slot(joiner, routing).await.unwrap();
    match &reply {
        ManagerReply::SlotsGranted { slots, already } => {
            assert!(!already);
            assert_eq!(slots.len(), 1);
            assert_eq!((slots[0].slot, slots[0].g), (routing, 1));
            assert_eq!(slots[0].root, (0, 0), "never minted");
        }
        other => panic!("{other:?}"),
    }
    // Tree 0 records the lessee; the joiner's page names the slot.
    let states = tree0_states(&vol).await;
    assert!(matches!(
        states.iter().find(|(s, _)| *s == fslot).map(|(_, st)| st),
        Some(SlotState::Leased { appender_id, g: 1, .. }) if *appender_id == joiner
    ));
    let entries = read_directory(std::path::Path::new(&uris[0]), vol.superblock())
        .await
        .unwrap();
    let page = entries
        .iter()
        .find(|e| e.appender_id == joiner)
        .and_then(|e| e.page.clone())
        .expect("the joiner's page");
    assert!(page
        .slots
        .iter()
        .any(|e| e.slot == routing && e.g == 1 && e.state == SlotEntryState::Live));
    // The manager (appender 0) asks for the same slot: refused, holder named.
    match vol.manager_acquire_slot(0, fslot).await.unwrap() {
        AcquireSlotReply::Refused { holder, g } => assert_eq!((holder, g), (joiner, 1)),
        other => panic!("{other:?}"),
    }
    // The joiner's replay: already.
    match client.acquire_slot(joiner, routing).await.unwrap() {
        ManagerReply::SlotsGranted { already, slots } => {
            assert!(already);
            assert_eq!(slots[0].g, 1, "a replay never moves g");
        }
        other => panic!("{other:?}"),
    }
    // Resolution: the manager's view and the wire verb agree.
    match client.resolve_slot(routing).await.unwrap() {
        ManagerReply::Holder { appender_id, g } => assert_eq!((appender_id, g), (joiner, 1)),
        other => panic!("{other:?}"),
    }
    match client.resolve_slot(101).await.unwrap() {
        ManagerReply::Unleased { g: 0 } => {}
        other => panic!("{other:?}"),
    }
    let plane = vol.slot_leases().expect("armed");
    assert_eq!(
        plane.holders.holder(fslot).map(|h| (h.appender_id, h.g)),
        Some((joiner, 1)),
        "the holder cache is tree 0's projection"
    );
    assert_eq!(lease_stats(&vol).resolve_rpcs, 0, "warm path: no resolve RPC");
    // The S4 table: that slot is FOREIGN, everything else local.
    assert_eq!(squeezefs::dlm_slot::dlm_mode(), "slot-homed");
    assert!(!squeezefs::dlm_slot::is_local_slot(u64::from(routing)));
    assert!(squeezefs::dlm_slot::is_local_slot(1));
    assert_eq!(squeezefs::dlm_slot::dlm_rpcs(), 0);
    assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    host.shutdown();
    shutdown(&routed).await;
}

/// §5.4.1 — the partition law's two faces: a COMMIT naming a slot a wire
/// joiner leases refuses EAGAIN-class at the door naming the holder (the
/// metanode arm's ship is PR 6/12's), and a leaf mutation that bypasses
/// the door — a direct tree insert into the foreign slot's tree — is
/// refused at `apply_locked` (`meta_kv_leaf_lease_refusals`, the belt).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaf_lease_refusals_fire_on_a_foreign_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let (host, mut client, joiner) = wire_joiner(&vol, 2).await;
    let routing: u16 = 200;
    let fslot = guest_forest_slot(routing);
    // The joiner leases the slot; the manager had minted a tree there
    // before (an inherited history — a record of a prior era).
    let tag = 0xFEED;
    let own = ino_in_slot(fslot, 7);
    vol.commit_block_refs(own, &refs(tag, own, 0, 2)).await.unwrap();
    assert_eq!(vol.block_ref_count(tag, 0).await.unwrap(), 1);
    // The manager took the slot first-writer-takes-it at that commit.
    let plane = vol.slot_leases().expect("armed");
    assert!(plane.gate.is_leased(fslot));
    // Hand it to the joiner: release + the joiner's acquire.
    vol.release_slot_handover(0, fslot).await.unwrap();
    assert!(!plane.gate.is_leased(fslot));
    match client.acquire_slot(joiner, routing).await.unwrap() {
        ManagerReply::SlotsGranted { slots, .. } => {
            assert_eq!(slots[0].g, 2, "g moved at the grant");
            assert_ne!(slots[0].root, (0, 0), "the grant carries the tree's root");
        }
        other => panic!("{other:?}"),
    }
    let before = META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed);
    // Face 1: the commit door refuses, naming the holder.
    let e = vol
        .commit_block_refs(own, &refs(tag, own, 10, 1))
        .await
        .err()
        .expect("a foreign slot's mutation refuses");
    let msg = e.to_string();
    assert!(msg.contains(&format!("appender {joiner}")), "{msg}");
    assert!(msg.contains("ship"), "{msg}");
    assert_eq!(META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed), before, "the door, not the gate");
    // Face 2: a leaf mutation past the door reaches the gate.
    let tree = vol
        .all_trees()
        .into_iter()
        .find(|t| t.forest_slot() == Some(fslot))
        .expect("the tree exists");
    let key = squeezefs::meta_backend::kv::record::forest_key(
        squeezefs::meta_backend::kv::record::TREE_INODES,
        &own.to_be_bytes(),
    )
    .unwrap();
    let e = tree
        .insert(&key, bytes::Bytes::from_static(b"x"))
        .await
        .err()
        .expect("the gate refuses a foreign leaf");
    assert!(e.to_string().contains("does not lease"), "{e}");
    assert_eq!(META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed), before + 1);
    // The history is intact and readable.
    assert_eq!(vol.block_ref_count(tag, 0).await.unwrap(), 1);
    host.shutdown();
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// §5.1.4 — handover: flush-then-transfer between two live appenders, the
// one-ring-per-key invariant, dominance, the crowd, the cooldown.
// ---------------------------------------------------------------------------

/// Region 1 (the seam's appender, leasing slot 4) holds records in ITS
/// ring; the slot is offered to the manager and accepted: flush-then-
/// transfer — tree 0 `Unleased { root, cursor, g, extents, tails }` then
/// `Leased { 0, g + 1 }`, region 1's page without the slot, region 0's
/// with it — and every later record of the slot rides ring 0. A crash
/// remount replays both rings with `meta_kv_replay_key_violations == 0`
/// (one ring per key in-window) and serves every acked record; the
/// handover's phases are recorded exact-sum.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handover_never_leaves_in_window_records_behind() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    let tag = volume_tag("vol-0000000000000004");
    let owner = ino_in_slot(SLOT4, 5);
    let (digest_before, g_after) = {
        let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
        let vol = Arc::clone(&routed.volumes[0]);
        let plane = vol.slot_leases().expect("armed");
        assert!(plane.gate.is_leased(SLOT4));
        let set = vol.appender_stats().unwrap();
        assert_eq!(set.regions.len(), 2);
        assert_eq!(set.regions[1].leases, 1, "region 1 leases slot 4 through the real acquire");
        let states = tree0_states(&vol).await;
        assert!(matches!(
            states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st),
            Some(SlotState::Leased { appender_id: 1, g: 1, .. })
        ));
        // Region 1's records — its ring.
        let ring1_before = set.regions[1].ring_entries;
        for i in 0..6 {
            vol.commit_block_refs(owner, &refs(tag, owner, i * 10, 4))
                .await
                .unwrap();
        }
        let set = vol.appender_stats().unwrap();
        assert!(set.regions[1].ring_entries >= ring1_before + 6, "{set:?}");
        // The offer (holder 1 → requester 0) and the accept.
        vol.manager_offer_slot(1, SLOT4, 0).await.unwrap();
        let s = lease_stats(&vol);
        assert_eq!(s.offers, 1);
        let reply = vol.manager_acquire_slot(0, SLOT4).await.unwrap();
        let AcquireSlotReply::Granted(grant) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(grant.g, 2, "g moved once at the grant");
        assert_ne!(grant.words.root, (0, 0), "the tree travelled with its root");
        assert!(grant.words.cursor >= 6 || grant.words.cursor == 0);
        let s = lease_stats(&vol);
        assert_eq!(s.handovers, 1);
        assert_eq!(s.offers, s.handovers + s.offers_expired, "the closure law");
        assert_eq!(
            s.handover_phase_ns[4],
            s.handover_phase_ns[..4].iter().sum::<u64>(),
            "flush + page + tree0 + grant ≡ total"
        );
        assert!(s.handover_phase_ns[0] > 0 && s.handover_phase_ns[2] > 0);
        eprintln!(
            "handover row (dev box — scoping, both arms in-process): flush {} µs, page {} µs, \
             tree0 {} µs, grant {} µs, total {} µs",
            s.handover_phase_ns[0] / 1000,
            s.handover_phase_ns[1] / 1000,
            s.handover_phase_ns[2] / 1000,
            s.handover_phase_ns[3] / 1000,
            s.handover_phase_ns[4] / 1000
        );
        // Tree 0: Leased { 0, g = 2 }; region 1's page dropped the slot,
        // region 0's page names it at the next checkpoint.
        let states = tree0_states(&vol).await;
        assert!(matches!(
            states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st),
            Some(SlotState::Leased { appender_id: 0, g: 2, .. })
        ));
        let set = vol.appender_stats().unwrap();
        assert_eq!(set.regions[1].leases, 0);
        assert!(!plane.gate.is_releasing(SLOT4) && plane.gate.is_leased(SLOT4));
        vol.checkpoint_now().await.unwrap();
        let entries = read_directory(path, vol.superblock()).await.unwrap();
        let p1 = entries[1].page.clone().unwrap();
        assert!(p1.slots.iter().all(|e| guest_forest_slot(e.slot) != SLOT4));
        let p0 = entries[0].page.clone().unwrap();
        let e0 = p0
            .slots
            .iter()
            .find(|e| guest_forest_slot(e.slot) == SLOT4)
            .expect("region 0's page names the slot");
        assert_eq!((e0.g, e0.state), (2, SlotEntryState::Live));
        assert!(e0.slot_tree_extents >= 1);
        // Later records of the slot ride ring 0.
        let ring0_before = vol.journal_ring().written_entries();
        let ring1_after = vol.appender_stats().unwrap().regions[1].ring_entries;
        for i in 6..10 {
            vol.commit_block_refs(owner, &refs(tag, owner, i * 10, 4))
                .await
                .unwrap();
        }
        assert!(vol.journal_ring().written_entries() >= ring0_before + 4);
        assert_eq!(
            vol.appender_stats().unwrap().regions[1].ring_entries,
            ring1_after,
            "region 1's ring took nothing after the handover"
        );
        for i in 0..10 {
            assert_eq!(vol.block_ref_count(tag, i * 10).await.unwrap(), 1, "block {i}");
        }
        let d = digest_backend(&vol).await.unwrap();
        vol.sync_device().await.unwrap();
        // The crash: no shutdown, no leave.
        drop(vol);
        drop(routed);
        (d, grant.g)
    };
    let key_before = META_KV_REPLAY_KEY_VIOLATIONS.load(Ordering::Relaxed);
    let lease_before = META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed);
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(META_KV_REPLAY_KEY_VIOLATIONS.load(Ordering::Relaxed), key_before);
    assert_eq!(META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed), lease_before);
    assert!(vol.appender_stats().unwrap().self_recoveries >= 1);
    for i in 0..10 {
        assert_eq!(vol.block_ref_count(tag, i * 10).await.unwrap(), 1, "block {i}");
    }
    assert_eq!(digest_backend(&vol).await.unwrap(), digest_before);
    // The slot stays region 0's at g = 2 across the crash; the seam's
    // declared slot is region 1's again only if unleased — it is not.
    let states = tree0_states(&vol).await;
    assert!(matches!(
        states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st),
        Some(SlotState::Leased { appender_id: 0, g, .. }) if *g == g_after
    ));
    shutdown(&routed).await;
}

/// The holder's dominance rule (§5.1.4) at the served ships: a live
/// holder is never recalled by a touch; a single touch never moves an
/// idle tree; a paused live job keeps its tree; twelve thousand
/// single-shot creators never move the directory slot; ONE dominating
/// requester gets the offer and the crowd does not; a bursty job reclaims
/// its tree from a trickle holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dominance_over_a_common_window_decides_every_offer() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    let n_floor = plane.n_floor();
    assert!(n_floor >= 2, "the floor: a single touch never moves anything");
    let tag = 0xD0D0;
    // The LIVE holder (region 0) commits into rotor slot 2 (routing 1).
    let live_slot: ForestSlot = 2;
    let live_owner = ino_in_slot(live_slot, 9);
    for i in 0..20 {
        vol.commit_block_refs(live_owner, &refs(tag, live_owner, i, 1))
            .await
            .unwrap();
    }
    // A touch by requester 7: served, never an offer.
    assert_eq!(vol.note_slot_ship(live_slot, 7).await, ShipVerdict::Serve);
    assert_eq!(lease_stats(&vol).offers, 0);
    // 39 more ships (< 2 × 20 ops): still served.
    for _ in 0..38 {
        assert_eq!(vol.note_slot_ship(live_slot, 7).await, ShipVerdict::Serve);
    }
    assert_eq!(lease_stats(&vol).offers, 0, "a live holder is never recalled by a touch");
    // A paused live job: no new ops, the window still counts its 20.
    for _ in 0..10 {
        assert_eq!(vol.note_slot_ship(live_slot, 8).await, ShipVerdict::Serve);
    }
    assert_eq!(lease_stats(&vol).offers, 0, "a paused live job keeps its tree");
    // An IDLE tree (rotor slot 3): one touch never moves it.
    let idle_slot: ForestSlot = 3;
    assert_eq!(vol.note_slot_ship(idle_slot, 11).await, ShipVerdict::Serve);
    assert_eq!(lease_stats(&vol).offers, 0, "a single touch never moves an idle tree");
    // Twelve thousand single-shot creators on the idle tree: aggregate
    // shipping never triggers.
    for q in 1_000..13_000u32 {
        assert_eq!(vol.note_slot_ship(idle_slot, q).await, ShipVerdict::Serve);
    }
    let s = lease_stats(&vol);
    assert_eq!(s.offers, 0, "twelve thousand single-shot creators never move the slot");
    assert_eq!(s.ships, 39 + 10 + 1 + 12_000);
    // ONE dominating requester (appender 1) reaches N_floor on the idle
    // tree: the IDLE arm offers to it and nobody else.
    for i in 0..n_floor {
        let v = vol.note_slot_ship(idle_slot, 1).await;
        if i + 1 < n_floor {
            assert_eq!(v, ShipVerdict::Serve);
        } else {
            assert_eq!(v, ShipVerdict::OfferIdle { to: 1 });
        }
    }
    let s = lease_stats(&vol);
    assert_eq!((s.offers, s.offers_idle, s.offers_dominated), (1, 1, 0));
    // The crowd keeps being served while the offer stands.
    assert_eq!(vol.note_slot_ship(idle_slot, 12_345).await, ShipVerdict::Serve);
    assert_eq!(lease_stats(&vol).offers, 1);
    // The bursty job reclaims its tree from a trickle holder: the holder
    // trickles 5 ops on rotor slot 5; requester 1 bursts — at the
    // max(2 × 5, N_floor)-th ship the DOMINATED arm offers.
    let trickle_slot: ForestSlot = 5;
    let trickle_owner = ino_in_slot(trickle_slot, 3);
    for i in 0..5 {
        vol.commit_block_refs(trickle_owner, &refs(tag, trickle_owner, 500 + i, 1))
            .await
            .unwrap();
    }
    let need = (2 * 5).max(n_floor);
    for i in 0..need {
        let v = vol.note_slot_ship(trickle_slot, 1).await;
        if i + 1 < need {
            assert_eq!(v, ShipVerdict::Serve, "ship {}", i + 1);
        } else {
            assert_eq!(v, ShipVerdict::OfferDominated { to: 1 });
        }
    }
    let s = lease_stats(&vol);
    assert_eq!((s.offers, s.offers_idle, s.offers_dominated), (2, 1, 1));
    // The accept: appender 1's AcquireSlot → flush-then-transfer → g + 1.
    let reply = vol.manager_acquire_slot(1, trickle_slot).await.unwrap();
    let AcquireSlotReply::Granted(g) = reply else {
        panic!("{reply:?}");
    };
    assert_eq!(g.g, 2);
    let s = lease_stats(&vol);
    assert_eq!(s.handovers, 1);
    assert_eq!(s.offers, s.handovers + s.offers_expired + 1, "one offer still open (the idle one)");
    assert_eq!(vol.appender_stats().unwrap().regions[1].leases, 2, "slots 4 and 5");
    for i in 0..5 {
        assert_eq!(vol.block_ref_count(tag, 500 + i).await.unwrap(), 1);
    }
    shutdown(&routed).await;
}

/// Two nodes alternating on one directory converge on ONE holder: the
/// requester-side cooldown (the S10 valve over `T_idle`) serves the
/// alternating touches after the first handover instead of re-offering.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_alternating_on_one_directory_converge_on_one_holder() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    // T_idle = 1 s ⇒ cooldown = 8 s (the valve's 8 windows).
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION).t_idle_ms("1000")).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    assert_eq!(plane.t_idle_ms, 1000);
    let n_floor = plane.n_floor();
    let slot: ForestSlot = 6; // an idle rotor slot of region 0
    // Node 1 bursts: the idle arm offers, node 1 accepts — handover 1.
    for _ in 0..n_floor {
        vol.note_slot_ship(slot, 1).await;
    }
    assert_eq!(lease_stats(&vol).offers, 1);
    let AcquireSlotReply::Granted(g) = vol.manager_acquire_slot(1, slot).await.unwrap() else {
        panic!()
    };
    assert_eq!(g.g, 2);
    assert!(plane.in_cooldown(slot, squeezefs::mono_core::monotonic_ns_u64()));
    // Now node 0 (the manager's own ops would be commits; as a REQUESTER
    // it ships) and node 1 alternate: inside the cooldown every ship is
    // served — no second offer, no second handover.
    for _ in 0..(4 * n_floor) {
        assert_eq!(vol.note_slot_ship(slot, 0).await, ShipVerdict::Serve);
        assert_eq!(vol.note_slot_ship(slot, 1).await, ShipVerdict::Serve);
    }
    let s = lease_stats(&vol);
    assert_eq!((s.handovers, s.offers), (1, 1), "the pair converged on one holder");
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// §5.3.4 rows 5–8 — the handover's crash windows through the seams.
// ---------------------------------------------------------------------------

/// Row 5: the holder dies after its page named the slot `Releasing {
/// root, cursor, g }` and BEFORE tree 0 was written. The next open of
/// that identity completes the release FROM THE PAGE — tree 0 `Unleased`
/// at the page's `g` with the page's root and cursor (§5.1.8) — serves
/// every acked record, counts no conflict, and a fresh acquire takes the
/// tree at `g + 1` with that root.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_holder_dying_after_its_page_named_releasing_has_tree0_written_from_the_page() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    let tag = volume_tag("vol-0000000000000005");
    let owner = ino_in_slot(SLOT4, 5);
    let (page_entry, digest_before) = {
        let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
        let vol = Arc::clone(&routed.volumes[0]);
        for i in 0..6 {
            vol.commit_block_refs(owner, &refs(tag, owner, i * 10, 4))
                .await
                .unwrap();
        }
        TEST_HANDOVER_HOLD_AFTER_PAGE.store(true, Ordering::Relaxed);
        let e = vol
            .release_slot_handover(1, SLOT4)
            .await
            .err()
            .expect("the seam kills the holder");
        TEST_HANDOVER_HOLD_AFTER_PAGE.store(false, Ordering::Relaxed);
        assert!(e.to_string().contains("TEST_HANDOVER_HOLD_AFTER_PAGE"), "{e}");
        // Durable state: page 1 names slot 4 Releasing at g = 1 with the
        // flushed root; tree 0 still Leased { 1, g: 1 }.
        let entries = read_directory(path, vol.superblock()).await.unwrap();
        let p1 = entries[1].page.clone().unwrap();
        let se = p1
            .slots
            .iter()
            .find(|e| guest_forest_slot(e.slot) == SLOT4)
            .copied()
            .expect("the Releasing entry");
        assert_eq!((se.state, se.g), (SlotEntryState::Releasing, 1));
        assert_ne!(se.root.addr, 0, "the flushed tree's root travels on the page");
        let states = tree0_states(&vol).await;
        assert!(matches!(
            states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st),
            Some(SlotState::Leased { appender_id: 1, g: 1, .. })
        ));
        let d = digest_backend(&vol).await.unwrap();
        vol.sync_device().await.unwrap();
        drop(vol);
        drop(routed);
        (se, d)
    };
    // The next open of this identity — region 1 declares another slot,
    // so nothing re-acquires slot 4 behind the settle.
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION_ALT)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(lease_stats(&vol).conflicts, 0);
    let states = tree0_states(&vol).await;
    match states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st) {
        Some(SlotState::Unleased {
            root, cursor, g, ..
        }) => {
            assert_eq!(*g, 1, "the page's g");
            assert_eq!(
                (root.addr, root.seq),
                (page_entry.root.addr, page_entry.root.seq),
                "the page's root"
            );
            assert_eq!(*cursor, page_entry.cursor, "the page's cursor (§5.1.8)");
        }
        other => panic!("{other:?}"),
    }
    let plane = vol.slot_leases().expect("armed");
    assert!(!plane.gate.is_leased(SLOT4));
    assert!(plane.gate.is_leased(SLOT_ALT), "region 1's declared slot");
    assert_eq!(vol.appender_stats().unwrap().regions[1].leases, 1);
    for i in 0..6 {
        assert_eq!(vol.block_ref_count(tag, i * 10).await.unwrap(), 1, "block {i}");
    }
    assert_eq!(digest_backend(&vol).await.unwrap(), digest_before);
    // A fresh acquire takes the tree at g + 1 with the page's root.
    let g = vol
        .manager_acquire_slots(0, 0, &[SLOT4])
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(g.g, 2);
    assert_eq!(g.words.root, (page_entry.root.addr, page_entry.root.seq));
    vol.commit_block_refs(owner, &refs(tag, owner, 100, 1))
        .await
        .unwrap();
    assert_eq!(vol.block_ref_count(tag, 100).await.unwrap(), 1);
    shutdown(&routed).await;
}

/// Row 6: the holder dies AFTER the manager's tree-0 ack and BEFORE its
/// page dropped the slot — page `Releasing` ∧ tree 0 `Unleased` (first
/// half), then page `Releasing` ∧ tree 0 `Leased { other }` (second
/// half). Tree 0 wins both times: the stale entry is dropped, never
/// completed, never a conflict, and the acked records are served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tree0_wins_over_a_stale_releasing_page_entry() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    let tag = volume_tag("vol-0000000000000006");
    let owner = ino_in_slot(SLOT4, 5);
    // ---- first half: tree 0 Unleased.
    {
        let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
        let vol = Arc::clone(&routed.volumes[0]);
        for i in 0..4 {
            vol.commit_block_refs(owner, &refs(tag, owner, i * 10, 2))
                .await
                .unwrap();
        }
        TEST_HANDOVER_HOLD_AFTER_TREE0.store(true, Ordering::Relaxed);
        let e = vol
            .release_slot_handover(1, SLOT4)
            .await
            .err()
            .expect("the seam kills the holder");
        TEST_HANDOVER_HOLD_AFTER_TREE0.store(false, Ordering::Relaxed);
        assert!(e.to_string().contains("TEST_HANDOVER_HOLD_AFTER_TREE0"), "{e}");
        let entries = read_directory(path, vol.superblock()).await.unwrap();
        let p1 = entries[1].page.clone().unwrap();
        assert!(p1
            .slots
            .iter()
            .any(|e| guest_forest_slot(e.slot) == SLOT4 && e.state == SlotEntryState::Releasing));
        let states = tree0_states(&vol).await;
        assert!(matches!(
            states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st),
            Some(SlotState::Unleased { g: 1, .. })
        ));
        vol.sync_device().await.unwrap();
        drop(vol);
        drop(routed);
    }
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION_ALT)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(lease_stats(&vol).conflicts, 0);
    let states = tree0_states(&vol).await;
    assert!(
        matches!(
            states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st),
            Some(SlotState::Unleased { g: 1, .. })
        ),
        "tree 0 wins: the release stands at g = 1, never re-completed"
    );
    let plane = vol.slot_leases().expect("armed");
    assert!(!plane.gate.is_leased(SLOT4));
    for i in 0..4 {
        assert_eq!(vol.block_ref_count(tag, i * 10).await.unwrap(), 1);
    }
    vol.checkpoint_now().await.unwrap();
    let entries = read_directory(path, vol.superblock()).await.unwrap();
    let p1 = entries[1].page.clone().unwrap();
    assert!(
        p1.slots.iter().all(|e| guest_forest_slot(e.slot) != SLOT4),
        "the stale entry is gone from the page"
    );
    // ---- second half: tree 0 Leased { other }. Region 1 takes slot 4
    // again explicitly (g = 2), the seam kills its handover after tree
    // 0's release (Unleased at g = 2 — a release never moves g), the
    // manager (region 0) acquires it — tree 0 Leased { 0, g = 3 } — and
    // the process dies before page 0 names it.
    let g = vol
        .manager_acquire_slots(1, 0, &[SLOT4])
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(g.g, 2);
    vol.checkpoint_now().await.unwrap();
    vol.commit_block_refs(owner, &refs(tag, owner, 200, 2))
        .await
        .unwrap();
    TEST_HANDOVER_HOLD_AFTER_TREE0.store(true, Ordering::Relaxed);
    let _ = vol
        .release_slot_handover(1, SLOT4)
        .await
        .err()
        .expect("the seam kills the holder");
    TEST_HANDOVER_HOLD_AFTER_TREE0.store(false, Ordering::Relaxed);
    let g = vol
        .manager_acquire_slots(0, 0, &[SLOT4])
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(g.g, 3, "Unleased at 2 → Leased {{ 0 }} at 3");
    let entries = read_directory(path, vol.superblock()).await.unwrap();
    assert!(entries[1]
        .page
        .clone()
        .unwrap()
        .slots
        .iter()
        .any(|e| guest_forest_slot(e.slot) == SLOT4 && e.state == SlotEntryState::Releasing));
    assert!(entries[0]
        .page
        .clone()
        .unwrap()
        .slots
        .iter()
        .all(|e| guest_forest_slot(e.slot) != SLOT4));
    vol.sync_device().await.unwrap();
    drop(vol);
    drop(routed);
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION_ALT)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(lease_stats(&vol).conflicts, 0);
    let states = tree0_states(&vol).await;
    assert!(matches!(
        states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st),
        Some(SlotState::Leased { appender_id: 0, g: 3, .. })
    ));
    let plane = vol.slot_leases().expect("armed");
    assert!(plane.gate.is_leased(SLOT4) && !plane.gate.is_releasing(SLOT4));
    assert_eq!(vol.appender_stats().unwrap().regions[1].leases, 1, "the alternate slot only");
    for i in 0..4 {
        assert_eq!(vol.block_ref_count(tag, i * 10).await.unwrap(), 1);
    }
    assert_eq!(vol.block_ref_count(tag, 200).await.unwrap(), 1);
    vol.commit_block_refs(owner, &refs(tag, owner, 300, 1))
        .await
        .unwrap();
    vol.checkpoint_now().await.unwrap();
    let entries = read_directory(path, vol.superblock()).await.unwrap();
    let p0 = entries[0].page.clone().unwrap();
    assert!(p0
        .slots
        .iter()
        .any(|e| guest_forest_slot(e.slot) == SLOT4 && e.g == 3 && e.state == SlotEntryState::Live));
    assert!(entries[1]
        .page
        .clone()
        .unwrap()
        .slots
        .iter()
        .all(|e| guest_forest_slot(e.slot) != SLOT4));
    shutdown(&routed).await;
}

/// Rows 7 and 8 (§5.3.5): a WIRE requester's accepted offer dies with the
/// manager after tree 0 wrote the holder's release and before the
/// requester's page named the slot — the requester retries `AcquireSlot`
/// against the successor manager and is granted, idempotently, with no
/// refusal (row 7); a requester that dies after its page named a slot
/// keeps it — tree 0 `Leased { requester }`, the manager's own commit
/// into it refuses naming the holder, and the requester's own re-join
/// re-adopts it as `already` at the same `g` (row 8).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requester_retries_against_the_successor_and_a_dead_requesters_lease_survives() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0000000000000007");
    // Rotor slot 6 (routing 5) holds the manager's records.
    let slot: ForestSlot = 6;
    let routing: u16 = 5;
    let owner = ino_in_slot(slot, 3);
    let joiner = {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let vol = Arc::clone(&routed.volumes[0]);
        let (host, mut client, joiner) = wire_joiner(&vol, 3).await;
        for i in 0..4 {
            vol.commit_block_refs(owner, &refs(tag, owner, i * 10, 2))
                .await
                .unwrap();
        }
        // The joiner's ships dominate the idle holder: the offer.
        let n_floor = vol.slot_leases().unwrap().n_floor();
        let need = (2 * 4).max(n_floor);
        for _ in 0..need {
            vol.note_slot_ship(slot, joiner).await;
        }
        assert_eq!(lease_stats(&vol).offers, 1);
        // The accept dies with the manager after tree 0 wrote the release.
        TEST_HANDOVER_HOLD_AFTER_TREE0.store(true, Ordering::Relaxed);
        let r = client.acquire_slot(joiner, routing).await;
        TEST_HANDOVER_HOLD_AFTER_TREE0.store(false, Ordering::Relaxed);
        assert!(r.is_err() || !matches!(r, Ok(ManagerReply::SlotsGranted { .. })), "{r:?}");
        let states = tree0_states(&vol).await;
        assert!(matches!(
            states.iter().find(|(s, _)| *s == slot).map(|(_, st)| st),
            Some(SlotState::Unleased { g: 1, .. })
        ));
        vol.sync_device().await.unwrap();
        host.shutdown();
        drop(vol);
        drop(routed);
        joiner
    };
    // Row 7: the successor manager; the requester retries.
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(lease_stats(&vol).conflicts, 0);
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        ManagerService::new(Arc::clone(&vol)),
    )
    .unwrap();
    let mut client = ManagerClient::connect(&host.endpoint().to_string(), SECRET, "joiner-3")
        .await
        .unwrap();
    // The joiner's page is Live in the directory: its re-join is `already`.
    match client.join(joiner_identity(3), 0).await.unwrap() {
        ManagerReply::Joined {
            appender_id,
            already,
            ..
        } => assert_eq!((appender_id, already), (joiner, true)),
        other => panic!("{other:?}"),
    }
    let g = match client.acquire_slot(joiner, routing).await.unwrap() {
        ManagerReply::SlotsGranted { slots, already } => {
            assert!(!already, "the release landed, the grant had not: a fresh grant");
            assert_eq!(slots.len(), 1);
            assert_eq!(slots[0].g, 2);
            assert_ne!(slots[0].root, (0, 0), "the flushed tree travels");
            slots[0].g
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    let states = tree0_states(&vol).await;
    assert!(matches!(
        states.iter().find(|(s, _)| *s == slot).map(|(_, st)| st),
        Some(SlotState::Leased { appender_id, g: 2, .. }) if *appender_id == joiner
    ));
    // Row 8: the requester dies (its client goes away; its page stays
    // Live naming the slot). The lease survives: the manager's commit
    // refuses naming the holder, the holder cache names it, and the
    // requester's next join + acquire answer `already` at the same g.
    drop(client);
    let e = vol
        .commit_block_refs(owner, &refs(tag, owner, 100, 1))
        .await
        .err()
        .expect("a foreign slot's mutation refuses");
    assert!(e.to_string().contains(&format!("appender {joiner}")), "{e}");
    let plane = vol.slot_leases().unwrap();
    assert_eq!(
        plane.holders.holder(slot).map(|h| (h.appender_id, h.g)),
        Some((joiner, g))
    );
    let mut client = ManagerClient::connect(&host.endpoint().to_string(), SECRET, "joiner-3-again")
        .await
        .unwrap();
    match client.join(joiner_identity(3), 0).await.unwrap() {
        ManagerReply::Joined { already, .. } => assert!(already),
        other => panic!("{other:?}"),
    }
    match client.acquire_slot(joiner, routing).await.unwrap() {
        ManagerReply::SlotsGranted { slots, already } => {
            assert!(already);
            assert_eq!(slots[0].g, g);
        }
        other => panic!("{other:?}"),
    }
    for i in 0..4 {
        assert_eq!(vol.block_ref_count(tag, i * 10).await.unwrap(), 1);
    }
    assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    host.shutdown();
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// §5.1.8 the cursor, §5.1.3 forced shrink / LRU / region release, C14.
// ---------------------------------------------------------------------------

/// §5.1.8: the slot's mint cursor travels with the lease as a FLOOR — a
/// slot released with its top inos deleted and re-acquired (in-process
/// and across a remount) never re-mints one of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slot_released_reacquired_with_its_top_inos_deleted_never_remints_an_ino() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0000000000000008");
    let slot: ForestSlot = 6;
    let routing: u16 = 5;
    let (minted, digest) = {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let vol = Arc::clone(&routed.volumes[0]);
        let mut minted = Vec::new();
        for _ in 0..3 {
            let local = vol.allocate_guest_ino(routing).unwrap();
            let ino = squeezefs::meta_backend::guest_local_ino(routing, local);
            vol.commit_block_refs(ino, &refs(tag, ino, local * 10, 1))
                .await
                .unwrap();
            minted.push((local, ino));
        }
        let top = minted.iter().map(|(l, _)| *l).max().unwrap();
        assert_eq!(vol.guest_cursor_snapshot(routing), Some(top + 1));
        // Delete the top two inos' records (their block refs).
        for (local, ino) in minted.iter().rev().take(2) {
            let op = BlockRefOp::released(BlockRef {
                vol_tag: tag,
                block_idx: local * 10,
                owner_ino: *ino,
                block_index: 0,
            });
            vol.commit_block_refs(*ino, &[op]).await.unwrap();
        }
        // Release: the cursor travels into tree 0.
        vol.release_slot_handover(0, slot).await.unwrap();
        assert_eq!(vol.guest_cursor_snapshot(routing), None, "the cursor left with the lease");
        let states = tree0_states(&vol).await;
        match states.iter().find(|(s, _)| *s == slot).map(|(_, st)| st) {
            Some(SlotState::Unleased { cursor, .. }) => assert_eq!(*cursor, top + 1),
            other => panic!("{other:?}"),
        }
        // Re-acquire in-process: the next mint is above the top.
        let g = vol
            .manager_acquire_slots(0, 0, &[slot])
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(g.words.cursor, top + 1);
        let next = vol.allocate_guest_ino(routing).unwrap();
        assert!(next > top, "{next} re-mints a deleted ino ≤ {top}");
        // Release again and leave cleanly; the remount re-adopts.
        vol.release_slot_handover(0, slot).await.unwrap();
        let d = digest_backend(&vol).await.unwrap();
        shutdown(&routed).await;
        (next, d)
    };
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(digest_backend(&vol).await.unwrap(), digest);
    let g = vol
        .manager_acquire_slots(0, 0, &[slot])
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(g.words.cursor, minted + 1, "the cursor after the last mint travelled");
    let next = vol.allocate_guest_ino(routing).unwrap();
    assert!(next > minted, "{next} re-mints across the remount");
    shutdown(&routed).await;
}

/// §5.1.3 forced shrink: the membership census grows past 512 writers so
/// the derived `M` falls (64 → 32); the cadence releases the idle rotor
/// slots beyond it (least extents first, the handover's own sequence —
/// tree 0 `Unleased` with cursor and root), keeps every slot with live
/// ops, and `slot_rotor` reads the new `M`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forced_shrink_releases_idle_rotor_slots_down_to_the_derived_m() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    assert_eq!(lease_stats(&vol).rotor, 64);
    // Two rotor slots with live ops: they must survive the shrink even
    // when idle slots run out.
    let tag = volume_tag("vol-0000000000000009");
    let live: Vec<ForestSlot> = vec![2, 3];
    for s in &live {
        let owner = ino_in_slot(*s, 2);
        vol.commit_block_refs(owner, &refs(tag, owner, u64::from(*s) * 100, 2))
            .await
            .unwrap();
    }
    // 1,024 writers ⇒ M = clamp(65536 / 2048, 1, 64) = 32.
    plane.test_writers_known.store(1024, Ordering::Relaxed);
    vol.slot_lease_cadence().await.unwrap();
    let s = lease_stats(&vol);
    assert_eq!(s.rotor, 32);
    assert_eq!(s.forced_shrinks, 32);
    assert_eq!(s.leases_held, 1 + 32, "native + the shrunk rotor");
    for l in &live {
        assert!(plane.gate.is_leased(*l), "slot {l} has live ops");
    }
    let states = tree0_states(&vol).await;
    let unleased = states
        .iter()
        .filter(|(_, st)| matches!(st, SlotState::Unleased { g: 1, .. }))
        .count();
    assert_eq!(unleased, 32, "every released slot is Unleased at its g");
    // A second cadence is a no-op: nothing beyond M.
    vol.slot_lease_cadence().await.unwrap();
    assert_eq!(lease_stats(&vol).forced_shrinks, 32);
    // The census falls back: nothing grows back on its own (the rotor
    // refills through the mint policy's overflow arm, never the shrink).
    plane.test_writers_known.store(1, Ordering::Relaxed);
    vol.slot_lease_cadence().await.unwrap();
    let s = lease_stats(&vol);
    assert_eq!((s.rotor, s.forced_shrinks), (64, 32), "M in force is 64; the rotor holds 32");
    for i in 0..4 {
        for s in &live {
            let _ = i;
            assert_eq!(
                vol.block_ref_count(tag, u64::from(*s) * 100).await.unwrap(),
                1
            );
        }
    }
    shutdown(&routed).await;
}

/// §5.1.3 region release: a declared region whose last slot went (the
/// handover) and whose ring is drained goes `Free` at the cadence — the
/// page `Free` in both directory slots, `slot_region_releases` 1 — and a
/// remount over it is clean (the region rejoins on a fresh ring; the slot
/// stays the manager's).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declared_region_is_released_when_its_last_slot_goes() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    let tag = volume_tag("vol-000000000000000a");
    let owner = ino_in_slot(SLOT4, 5);
    let digest = {
        let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
        let vol = Arc::clone(&routed.volumes[0]);
        for i in 0..3 {
            vol.commit_block_refs(owner, &refs(tag, owner, i * 10, 2))
                .await
                .unwrap();
        }
        // The handover to the manager; the ring drains at the next covering cycle.
        vol.manager_offer_slot(1, SLOT4, 0).await.unwrap();
        let AcquireSlotReply::Granted(_) = vol.manager_acquire_slot(0, SLOT4).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(vol.appender_stats().unwrap().regions[1].leases, 0);
        vol.checkpoint_now().await.unwrap();
        vol.slot_lease_cadence().await.unwrap();
        let s = lease_stats(&vol);
        assert_eq!(s.region_releases, 1, "{:?}", vol.appender_stats().unwrap());
        let entries = read_directory(path, vol.superblock()).await.unwrap();
        let p1 = entries[1].page.clone().unwrap();
        assert_eq!(p1.state, AppenderState::Free);
        assert!(p1.slots.is_empty() && p1.segments.is_empty());
        assert_eq!(vol.appender_stats().unwrap().live, 1, "region 0 alone is joined");
        // The manager keeps serving the slot.
        vol.commit_block_refs(owner, &refs(tag, owner, 100, 1))
            .await
            .unwrap();
        for i in 0..3 {
            assert_eq!(vol.block_ref_count(tag, i * 10).await.unwrap(), 1);
        }
        let d = digest_backend(&vol).await.unwrap();
        shutdown(&routed).await;
        d
    };
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(digest_backend(&vol).await.unwrap(), digest);
    assert_eq!(lease_stats(&vol).conflicts, 0);
    let set = vol.appender_stats().unwrap();
    assert_eq!(set.regions.len(), 2);
    assert_eq!(
        set.regions[1].leases, 1,
        "the clean leave unleased every slot; the wish-list takes slot 4 back on the region's \
         fresh ring"
    );
    assert_eq!(set.live, 2);
    let plane = vol.slot_leases().expect("armed");
    assert!(plane.gate.is_leased(SLOT4));
    assert_eq!(vol.block_ref_count(tag, 100).await.unwrap(), 1);
    let ring1_before = set.regions[1].ring_entries;
    vol.commit_block_refs(owner, &refs(tag, owner, 200, 1))
        .await
        .unwrap();
    assert!(vol.appender_stats().unwrap().regions[1].ring_entries > ring1_before);
    shutdown(&routed).await;
}

/// C14's live face (§5.8.5): two Live pages attesting ONE slot refuse the
/// mount loud, naming both appenders and the class; the volume is left
/// as it was (`slot_lease_conflicts` counted on the refusing open).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_live_pages_attesting_one_slot_refuse_the_mount() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    let sb = {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let vol = Arc::clone(&routed.volumes[0]);
        let (host, mut client, joiner) = wire_joiner(&vol, 4).await;
        // The joiner leases routing slot 300 — its page names it Live.
        match client.acquire_slot(joiner, 300).await.unwrap() {
            ManagerReply::SlotsGranted { .. } => {}
            other => panic!("{other:?}"),
        }
        let sb = vol.superblock().clone();
        host.shutdown();
        shutdown(&routed).await;
        sb
    };
    // Forge the conflict: a second Live page (a second joiner) also
    // attesting routing slot 300 — the shape a torn custody hand-off
    // between two dead appenders leaves.
    let entries = read_directory(path, &sb).await.unwrap();
    let victim = entries
        .iter()
        .find(|e| e.appender_id != 0 && e.page.as_ref().is_some_and(|p| p.slots.iter().any(|s| s.slot == 300)))
        .expect("the joiner's page");
    let mut forged = victim.page.clone().unwrap();
    forged.appender_id = victim.appender_id + 1;
    forged.identity = joiner_identity(5);
    let free = entries
        .iter()
        .find(|e| e.appender_id == forged.appender_id)
        .expect("the directory's next pair");
    for off in &free.dir_offsets {
        forged.generation += 1;
        squeezefs::meta_backend::kv::appender::write_page(path, *off, forged.encode().unwrap())
            .await
            .unwrap();
    }
    Knobs::armed().apply();
    std::env::remove_var(TEST_APPENDER_SLOTS_ENV);
    let r = open_routed_meta_set(&uris).await;
    Knobs::clear();
    let e = r.err().expect("two live attestations refuse the mount");
    let msg = e.to_string();
    assert!(msg.contains("two appender pages"), "{msg}");
    assert!(msg.contains("C14"), "{msg}");
    assert!(msg.contains(&format!("{}", victim.appender_id)), "{msg}");
    assert!(msg.contains(&format!("{}", forged.appender_id)), "{msg}");
}
