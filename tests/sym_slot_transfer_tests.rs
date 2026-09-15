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
    read_directory, write_page, AppenderIdentity, AppenderState, SlotEntryState, SLOT_PAGE_BUDGET,
    TEST_APPENDER_SLOTS_ENV,
};
use squeezefs::meta_backend::kv::backend::{
    AcquireSlotReply, ControlAdmit, KvMetaBackend, TEST_HANDOVER_HOLD_AFTER_PAGE,
    TEST_HANDOVER_HOLD_AFTER_TREE0,
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
    KvError, META_KV_CHECKPOINTS, META_KV_LEAF_LEASE_REFUSALS, META_KV_REPLAY_KEY_VIOLATIONS,
    META_KV_REPLAY_LEASE_VIOLATIONS,
};
use squeezefs::meta_backend::{
    open_routed_meta_set, plan_meta_slot_set, Metadata, RoutedMetaBackend, MINT_SPREAD,
};
use squeezefs::slot_lease_core::{ShipVerdict, MINT_SPREAD as CORE_SPREAD};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// The measured wall of one served ship the contracts feed `note_slot_
/// ship` (the S8 owner-side `total` of the frame; the fleet's fabric RTT
/// order). `N_floor` = ceil(handover / ship) then reads ≈ the design's
/// tens, never the raw handover nanoseconds (review round 2, Issue 7).
const SHIP_NS: u64 = 200_000;

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
    assert_eq!(
        CORE_SPREAD, MINT_SPREAD as u64,
        "the core's spread is the routed layer's"
    );
    assert_eq!(
        s.leases_held,
        1 + MINT_SPREAD as u64,
        "1 native + 64 rotor: {s:?}"
    );
    assert_eq!(s.ceiling_overflows, 0, "a solo mount never asks for a 65th");
    assert_eq!(s.grants, 1 + MINT_SPREAD as u64);
    assert_eq!(s.conflicts, 0);
    assert_eq!(squeezefs::dlm_slot::dlm_mode(), "slot-homed");
    assert_eq!(
        squeezefs::dlm_slot::dlm_rpcs(),
        0,
        "solo: nothing is foreign"
    );
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
    assert_eq!(
        page0.slots.len(),
        1 + MINT_SPREAD,
        "the page names exactly the leases"
    );
    assert!(page0.slots.iter().all(|e| e.g == 1));
    assert!(page0.slots.iter().all(|e| e.state == SlotEntryState::Live));
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
    assert!(states.iter().any(
        |(_, st)| matches!(st, SlotState::Unleased { last_written, .. } if *last_written > 0)
    ));
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
    assert!(page0
        .slots
        .iter()
        .all(|e| e.g == 0 && e.slot_tree_extents == 0));
    assert_eq!(
        META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed),
        refusals_before,
        "the gate is inert unarmed"
    );
    // The seq-space law is inert here: the ring stamps its positions.
    assert_eq!(vol.journal_ring().seq_offset(), 0);
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
    assert_eq!(
        resolve_affinity_ceiling(640 << 20, 65_536, 1 << 30),
        10 << 20
    );
    std::env::set_var(SYM_AFFINITY_MAX_MB_ENV, "1");
    assert_eq!(
        resolve_affinity_ceiling(640 << 20, 65_536, 1 << 30),
        1 << 20
    );
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
    match squeezefs::env_knobs::lookup(SYM_MINT_SLOTS_ENV)
        .unwrap()
        .kind
    {
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
            .create(
                manydirs.ino,
                &format!("d{i:05}"),
                libc::S_IFDIR | 0o755,
                0,
                0,
            )
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
    let with_records = rotor.iter().filter(|s| plane.extents.get(**s) > 0).count() as u64;
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
    assert_eq!(
        slot_of_global(&routed, a.ino),
        job_slot,
        "affinity below the cap"
    );
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
    assert_ne!(
        b_slot, job_slot,
        "past the cap the child spills to the rotor"
    );
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
    assert_eq!(
        lease_stats(&vol).ceiling_overflows,
        0,
        "exact equality never overflows"
    );
    assert_eq!(plane.rotor.load().len(), 2);
    // Strictly over by one extent: the mint asks for one more (3rd), then
    // a 4th (= 2 × M); past it the smallest rotor tree.
    for round in 0..3 {
        for s in plane.rotor.load_full().iter() {
            plane.extents.set(*s, cap_extents + 1);
        }
        let f = routed
            .create(
                ROOT_INO,
                &format!("over{round}"),
                libc::S_IFREG | 0o644,
                0,
                0,
            )
            .await
            .unwrap();
        let slot = slot_of_global(&routed, f.ino);
        assert!(plane.gate.is_leased(slot));
        let s = lease_stats(&vol);
        match round {
            0 => {
                assert_eq!(s.ceiling_overflows, 1);
                assert_eq!(plane.rotor.load().len(), 3);
                assert_eq!(
                    *plane.rotor.load().last().unwrap(),
                    slot,
                    "minted in the new slot"
                );
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
    let mut client = ManagerClient::connect(&endpoint, SECRET, &format!("joiner-{n}"), 0)
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
            assert_eq!(slots[0].words.root, (0, 0), "never minted");
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
    assert_eq!(
        lease_stats(&vol).resolve_rpcs,
        2,
        "the two wire ResolveSlot verbs served — the manager's own resolution rode the \
         holder cache, never a verb"
    );
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
    vol.commit_block_refs(own, &refs(tag, own, 0, 2))
        .await
        .unwrap();
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
            assert_ne!(
                slots[0].words.root,
                (0, 0),
                "the grant carries the tree's root"
            );
        }
        other => panic!("{other:?}"),
    }
    let before = META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed);
    // Face 1: the commit door refuses, naming the holder.
    let e = vol
        .commit_block_refs(own, &refs(tag, own, 10, 1))
        .await
        .expect_err("a foreign slot's mutation refuses");
    let msg = e.to_string();
    assert!(msg.contains(&format!("appender {joiner}")), "{msg}");
    assert!(msg.contains("ship"), "{msg}");
    assert_eq!(
        META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed),
        before,
        "the door, not the gate"
    );
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
        .expect_err("the gate refuses a foreign leaf");
    assert!(e.to_string().contains("does not lease"), "{e}");
    assert_eq!(
        META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed),
        before + 1
    );
    // The history is intact and readable.
    assert_eq!(vol.block_ref_count(tag, 0).await.unwrap(), 1);
    host.shutdown();
    shutdown(&routed).await;
}

/// **Issue 6 (review round 2) — the door law on a legal schedule.** A
/// commit issued on the departing holder WHILE a handover is in flight
/// (the holder parked after its page named the slot `Releasing`, before
/// tree 0 moved) PARKS at the door — never an error — and completes when
/// the handover does: for an accept by a WIRE requester the slot is then
/// foreign and the commit refuses `SlotBusy` = EAGAIN (the "ship to the
/// holder" class), never `Corrupt`/EINVAL; for the cadence's own release
/// the slot is unleased and the commit re-acquires it first-touch and
/// SUCCEEDS. The belt (`meta_kv_leaf_lease_refusals`, must-stay-0) never
/// counts: the release drained the door's tokens before its flush.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commit_during_a_handover_parks_at_the_door_and_never_fails_einval() {
    use squeezefs::meta_backend::kv::backend::{
        test_handover_park_release, test_handover_parked, TEST_HANDOVER_PARK_AFTER_PAGE,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    let (host, _client, joiner) = wire_joiner(&vol, 6).await;
    let tag = 0xD006;
    let belt_before = META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed);
    let parked_before = test_handover_parked();

    // ---- Face 1: the accept by a wire requester — the slot leaves. ----
    let slot: ForestSlot = 2; // rotor slot 2 (routing 1)
    let owner = ino_in_slot(slot, 21);
    for i in 0..4u64 {
        vol.commit_block_refs(owner, &refs(tag, owner, i, 1))
            .await
            .unwrap();
    }
    vol.manager_offer_slot(0, slot, joiner).await.unwrap();
    TEST_HANDOVER_PARK_AFTER_PAGE.store(true, Ordering::Relaxed);
    let accept = {
        let vol = Arc::clone(&vol);
        tokio::spawn(async move { vol.manager_acquire_slot(joiner, slot).await })
    };
    // The handover parked: page Releasing, tree 0 not yet moved.
    while test_handover_parked() == parked_before {
        tokio::task::yield_now().await;
    }
    assert!(plane.gate.is_releasing(slot));
    // The commit issued INTO the window: it parks at the door.
    let commit = {
        let vol = Arc::clone(&vol);
        tokio::spawn(async move {
            vol.commit_block_refs(owner, &refs(tag, owner, 100, 1))
                .await
        })
    };
    while lease_stats(&vol).door_parks == 0 {
        assert!(!commit.is_finished(), "the commit must PARK, not fail");
        tokio::task::yield_now().await;
    }
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(
        !commit.is_finished(),
        "parked at the door while the handover is in flight"
    );
    // Release the handover: the requester is granted, the door re-reads.
    test_handover_park_release();
    let AcquireSlotReply::Granted(g) = accept.await.unwrap().unwrap() else {
        panic!("the accept lands")
    };
    assert_eq!(g.g, 2);
    let e = commit
        .await
        .unwrap()
        .expect_err("the slot is the wire joiner's now — ship to the holder");
    let msg = e.to_string();
    assert!(
        msg.contains(&format!("slot {slot} is leased by appender {joiner} (g 2)"))
            && msg.contains("ships to its holder"),
        "{msg}"
    );
    assert_eq!(
        e.to_errno(),
        libc::EAGAIN,
        "the retryable class, never EINVAL: {msg}"
    );
    let st = lease_stats(&vol);
    assert_eq!((st.door_parks, st.door_refusals), (1, 1));
    assert_eq!(vol.block_ref_count(tag, 100).await.unwrap(), 0);
    for i in 0..4u64 {
        assert_eq!(vol.block_ref_count(tag, i).await.unwrap(), 1);
    }

    // ---- Face 2: the cadence's own release — the slot stays home. ----
    let slot2: ForestSlot = 3;
    let owner2 = ino_in_slot(slot2, 22);
    vol.commit_block_refs(owner2, &refs(tag, owner2, 200, 1))
        .await
        .unwrap();
    let parked_before = test_handover_parked();
    TEST_HANDOVER_PARK_AFTER_PAGE.store(true, Ordering::Relaxed);
    let release = {
        let vol = Arc::clone(&vol);
        tokio::spawn(async move { vol.release_slot_handover(0, slot2).await })
    };
    while test_handover_parked() == parked_before {
        tokio::task::yield_now().await;
    }
    let commit = {
        let vol = Arc::clone(&vol);
        tokio::spawn(async move {
            vol.commit_block_refs(owner2, &refs(tag, owner2, 201, 1))
                .await
        })
    };
    while lease_stats(&vol).door_parks < 2 {
        assert!(!commit.is_finished(), "the commit must PARK, not fail");
        tokio::task::yield_now().await;
    }
    test_handover_park_release();
    release.await.unwrap().unwrap();
    commit
        .await
        .unwrap()
        .expect("an unleased slot is re-acquired first-touch — the commit lands");
    assert!(plane.gate.is_leased(slot2), "first-writer-takes-it again");
    assert_eq!(vol.block_ref_count(tag, 201).await.unwrap(), 1);
    let st = lease_stats(&vol);
    assert_eq!((st.door_parks, st.door_refusals), (2, 1));
    assert_eq!(
        META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed),
        belt_before,
        "the belt never fires on a legal schedule"
    );
    assert_eq!(
        vol.appender_stats().unwrap().manager_verb_refusals,
        0,
        "no witness contradiction happened"
    );
    // The handover's flush post-condition held: no record of the moved
    // slot is left in ring 0's window at the next open.
    host.shutdown();
    shutdown(&routed).await;
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(vol.block_ref_count(tag, 201).await.unwrap(), 1);
    shutdown(&routed).await;
}

/// **Issue 5 (review round 2) — one writer discipline per RAM set.** The
/// region's lease set and the plane's rotor are `ArcSwap`s mutated by
/// clone-and-store from different critical sections (a grant under
/// `manager_verbs`, a release under `handover`, the overflow arm under
/// none); every RMW now runs under the set's own writer mutex. The
/// storm: the cadence releases 40 leased slots (drops under `handover`)
/// while three committers take 60 fresh slots first-touch (adds under
/// `manager_verbs`) on the same region — afterwards the region's lease
/// set is EXACTLY the gate's (whose bits are `fetch_or`/`fetch_and`,
/// never lost), and the rotor is untouched by the storm. A lost update
/// would leave a released slot routed into ring 0 after tree 0 released
/// it (the `Lease` class at the next open) or a fresh slot's commits
/// routed away from the region that leases them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lease_set_and_rotor_rmws_never_lose_an_update() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    let tag = 0xD005;
    // 40 inherited slots (first-touch now, released by the storm).
    let inherited: Vec<ForestSlot> = (300..340).collect();
    for s in &inherited {
        let owner = ino_in_slot(*s, 3);
        vol.commit_block_refs(owner, &refs(tag, owner, u64::from(*s), 1))
            .await
            .unwrap();
    }
    let rotor_before = (**plane.rotor.load()).clone();
    let releaser = {
        let vol = Arc::clone(&vol);
        let inherited = inherited.clone();
        tokio::spawn(async move {
            for s in inherited {
                vol.release_slot_handover(0, s).await.unwrap();
            }
        })
    };
    let mut committers = Vec::new();
    for t in 0..3u32 {
        let vol = Arc::clone(&vol);
        committers.push(tokio::spawn(async move {
            for i in 0..20u32 {
                let slot: ForestSlot = 400 + t * 100 + i;
                let owner = ino_in_slot(slot, 5);
                vol.commit_block_refs(owner, &refs(tag, owner, 1_000 + u64::from(slot), 1))
                    .await
                    .unwrap();
            }
        }));
    }
    releaser.await.unwrap();
    for c in committers {
        c.await.unwrap();
    }
    let leased = plane.gate.leased_slots();
    assert_eq!(
        vol.appender_stats().unwrap().regions[0].leases,
        leased.len() as u64,
        "the region's lease set is exactly the gate's — no lost update"
    );
    for s in &inherited {
        assert!(!leased.contains(s), "slot {s} was released");
    }
    for t in 0..3u32 {
        for i in 0..20u32 {
            assert!(leased.contains(&(400 + t * 100 + i)));
        }
    }
    assert_eq!(
        &**plane.rotor.load(),
        &rotor_before,
        "the rotor is untouched"
    );
    // Every fresh slot's commit routes into ring 0 and lands.
    for t in 0..3u32 {
        let slot: ForestSlot = 400 + t * 100;
        let owner = ino_in_slot(slot, 5);
        vol.commit_block_refs(owner, &refs(tag, owner, 2_000 + u64::from(slot), 1))
            .await
            .unwrap();
        assert_eq!(
            vol.block_ref_count(tag, 2_000 + u64::from(slot))
                .await
                .unwrap(),
            1
        );
    }
    assert_eq!(META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed), 0);
    shutdown(&routed).await;
}

/// **The page-budget overflow law** (review round 2 — found by the Issue 5
/// storm): a region holding MORE slots than its page names (a burst of
/// first-touch acquires past `SLOT_PAGE_BUDGET`, none idle yet for the
/// LRU release) has roots its page cannot publish; an unpublished root is
/// a floor, so before the law the ledger tail pinned for good and the
/// clause-b audit FAIL-STOPPED the volume after 8 barriered cycles. The
/// overflow roots ride tree 0's `Leased` records instead: ten barriered
/// cycles pass, the tail advances past the burst, and a crash remount
/// resolves every overflow root off the record (the page names none).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_region_past_its_page_budget_publishes_the_overflow_roots_into_tree0() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let tag = 0xB0D6;
    let head_before = vol.journal_ring().core().head();
    // 120 inherited slots first-touched in one burst: 1 + 64 + 120 leases
    // on region 0, 77 past the page's 108.
    let touched: Vec<ForestSlot> = (1_000..1_120).collect();
    for s in &touched {
        let owner = ino_in_slot(*s, 9);
        vol.commit_block_refs(owner, &refs(tag, owner, u64::from(*s), 1))
            .await
            .unwrap();
    }
    assert!(
        vol.appender_stats().unwrap().regions[0].leases as usize > SLOT_PAGE_BUDGET,
        "the burst is past the budget"
    );
    for _ in 0..10 {
        vol.checkpoint_now()
            .await
            .expect("no wedge: the overflow roots are published into tree 0");
    }
    assert!(!vol.is_failed(), "never the clause-b fail-stop");
    assert!(
        vol.journal_ring().core().reusable_upto() > head_before,
        "the tail passed the burst"
    );
    // Every overflow slot's record names a live root; the page names it
    // not (the budget).
    let entries = read_directory(std::path::Path::new(&uris[0]), vol.superblock())
        .await
        .unwrap();
    let page0 = entries[0].page.clone().unwrap();
    assert_eq!(page0.slots.len(), SLOT_PAGE_BUDGET);
    let named: std::collections::BTreeSet<ForestSlot> = page0
        .slots
        .iter()
        .map(|e| guest_forest_slot(e.slot))
        .collect();
    let states = tree0_states(&vol).await;
    let mut overflow = 0;
    for s in &touched {
        if named.contains(s) {
            continue;
        }
        overflow += 1;
        assert!(
            matches!(
                states.iter().find(|(x, _)| x == s).map(|(_, st)| st),
                Some(SlotState::Leased { appender_id: 0, root, .. }) if root.addr != 0
            ),
            "slot {s}: the overflow root rides tree 0"
        );
    }
    assert!(overflow >= 77, "{overflow} overflow slots");
    // Crash remount: every record is served off the recovered roots.
    drop(vol);
    drop(routed);
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    for s in &touched {
        assert_eq!(vol.block_ref_count(tag, u64::from(*s)).await.unwrap(), 1);
    }
    assert_eq!(lease_stats(&vol).conflicts, 0);
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
        assert_eq!(
            set.regions[1].leases, 1,
            "region 1 leases slot 4 through the real acquire"
        );
        let states = tree0_states(&vol).await;
        assert!(matches!(
            states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st),
            Some(SlotState::Leased {
                appender_id: 1,
                g: 1,
                ..
            })
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
            Some(SlotState::Leased {
                appender_id: 0,
                g: 2,
                ..
            })
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
            assert_eq!(
                vol.block_ref_count(tag, i * 10).await.unwrap(),
                1,
                "block {i}"
            );
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
    assert_eq!(
        META_KV_REPLAY_KEY_VIOLATIONS.load(Ordering::Relaxed),
        key_before
    );
    assert_eq!(
        META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed),
        lease_before
    );
    assert!(vol.appender_stats().unwrap().self_recoveries >= 1);
    for i in 0..10 {
        assert_eq!(
            vol.block_ref_count(tag, i * 10).await.unwrap(),
            1,
            "block {i}"
        );
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
    assert!(
        n_floor >= 2,
        "the floor: a single touch never moves anything"
    );
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
    assert_eq!(
        vol.note_slot_ship(live_slot, 7, SHIP_NS).await,
        ShipVerdict::Serve
    );
    assert_eq!(lease_stats(&vol).offers, 0);
    // 39 more ships (< 2 × 20 ops): still served.
    for _ in 0..38 {
        assert_eq!(
            vol.note_slot_ship(live_slot, 7, SHIP_NS).await,
            ShipVerdict::Serve
        );
    }
    assert_eq!(
        lease_stats(&vol).offers,
        0,
        "a live holder is never recalled by a touch"
    );
    // A paused live job: no new ops, the window still counts its 20.
    for _ in 0..10 {
        assert_eq!(
            vol.note_slot_ship(live_slot, 8, SHIP_NS).await,
            ShipVerdict::Serve
        );
    }
    assert_eq!(
        lease_stats(&vol).offers,
        0,
        "a paused live job keeps its tree"
    );
    // An IDLE tree (rotor slot 3): one touch never moves it.
    let idle_slot: ForestSlot = 3;
    assert_eq!(
        vol.note_slot_ship(idle_slot, 11, SHIP_NS).await,
        ShipVerdict::Serve
    );
    assert_eq!(
        lease_stats(&vol).offers,
        0,
        "a single touch never moves an idle tree"
    );
    // Twelve thousand single-shot creators on the idle tree: aggregate
    // shipping never triggers.
    for q in 1_000..13_000u32 {
        assert_eq!(
            vol.note_slot_ship(idle_slot, q, SHIP_NS).await,
            ShipVerdict::Serve
        );
    }
    let s = lease_stats(&vol);
    assert_eq!(
        s.offers, 0,
        "twelve thousand single-shot creators never move the slot"
    );
    assert_eq!(s.ships, 39 + 10 + 1 + 12_000);
    // ONE dominating requester (appender 1) reaches N_floor on the idle
    // tree: the IDLE arm offers to it and nobody else. `N_floor` is LIVE
    // — every served ship feeds the ship-cost EWMA (Issue 7) — so the
    // verdict is checked against the floor in force AT the ship: the
    // offer fires at exactly the first ship where `ops_q ≥ N_floor`.
    let mut ops_q = 0u64;
    loop {
        ops_q += 1;
        let v = vol.note_slot_ship(idle_slot, 1, SHIP_NS).await;
        let nf = plane.n_floor();
        assert!((2..1_000).contains(&nf), "N_floor in force: {nf}");
        if ops_q >= nf {
            assert_eq!(
                v,
                ShipVerdict::OfferIdle { to: 1 },
                "ship {ops_q} ≥ N_floor {nf}"
            );
            break;
        }
        assert_eq!(v, ShipVerdict::Serve, "ship {ops_q} < N_floor {nf}");
        assert!(
            ops_q < 10_000,
            "no offer after {ops_q} ships (N_floor {nf})"
        );
    }
    let n_floor = plane.n_floor();
    let s = lease_stats(&vol);
    assert_eq!((s.offers, s.offers_idle, s.offers_dominated), (1, 1, 0));
    // The crowd keeps being served while the offer stands.
    assert_eq!(
        vol.note_slot_ship(idle_slot, 12_345, SHIP_NS).await,
        ShipVerdict::Serve
    );
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
    // The EWMA has converged on SHIP_NS by now (`N_floor` is stable);
    // the floor read here is the one every ship below evaluates against.
    let need = (2 * 5).max(n_floor);
    for i in 0..need {
        let v = vol.note_slot_ship(trickle_slot, 1, SHIP_NS).await;
        assert_eq!(plane.n_floor(), n_floor, "N_floor converged");
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
    assert_eq!(
        s.offers,
        s.handovers + s.offers_expired + 1,
        "one offer still open (the idle one)"
    );
    assert_eq!(
        vol.appender_stats().unwrap().regions[1].leases,
        2,
        "slots 4 and 5"
    );
    for i in 0..5 {
        assert_eq!(vol.block_ref_count(tag, 500 + i).await.unwrap(), 1);
    }
    shutdown(&routed).await;
}

/// Two nodes alternating on one directory converge on ONE holder: the
/// requester-side cooldown (the S10 valve over `T_idle`) serves the
/// alternating touches after the first handover instead of re-offering.
/// The pin runs ≥ 2 handovers against the REAL `N_floor` (review round 2,
/// Issue 7): after the first handover the floor is re-read and must
/// still be a count of ships (< 1,000), never the handover's wall in
/// nanoseconds — a second idle tree moves at exactly that floor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_alternating_on_one_directory_converge_on_one_holder() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    // T_idle = 1 s ⇒ cooldown = 8 s (the valve's 8 windows).
    let routed = open_under(
        &uris,
        &Knobs::armed().partition(PARTITION).t_idle_ms("1000"),
    )
    .await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = vol.slot_leases().expect("armed");
    assert_eq!(plane.t_idle_ms, 1000);
    // The cold-start seed: a handover is priced before the first one
    // runs, a ship before the first one is served — the floor is a
    // count of ships from the arm.
    let seeded = plane.n_floor();
    assert!(
        (2..1_000).contains(&seeded),
        "the seeded N_floor is a ship count: {seeded}"
    );
    let slot: ForestSlot = 6; // an idle rotor slot of region 0
                              // Node 1 bursts: the idle arm offers, node 1 accepts — handover 1.
    let mut ships = 0;
    loop {
        ships += 1;
        if vol.note_slot_ship(slot, 1, SHIP_NS).await != ShipVerdict::Serve {
            break;
        }
        assert!(ships < 10_000);
    }
    assert_eq!(lease_stats(&vol).offers, 1);
    let AcquireSlotReply::Granted(g) = vol.manager_acquire_slot(1, slot).await.unwrap() else {
        panic!()
    };
    assert_eq!(g.g, 2);
    assert!(plane.in_cooldown(slot, squeezefs::mono_core::monotonic_ns_u64()));
    // The REAL floor after a handover fed the handover EWMA: a ship count.
    let n_floor = plane.n_floor();
    assert!(
        (2..1_000).contains(&n_floor),
        "N_floor after the first handover is a ship count, not its wall in ns: {n_floor}"
    );
    // Now node 0 (the manager's own ops would be commits; as a REQUESTER
    // it ships) and node 1 alternate: inside the cooldown every ship is
    // served — no second offer, no second handover.
    for _ in 0..(4 * n_floor) {
        assert_eq!(
            vol.note_slot_ship(slot, 0, SHIP_NS).await,
            ShipVerdict::Serve
        );
        assert_eq!(
            vol.note_slot_ship(slot, 1, SHIP_NS).await,
            ShipVerdict::Serve
        );
    }
    let s = lease_stats(&vol);
    assert_eq!(
        (s.handovers, s.offers),
        (1, 1),
        "the pair converged on one holder"
    );
    // Handover 2 — another idle tree moves at the real floor (before the
    // feed, no offer could fire again on this mount: the floor read
    // ≈ 10⁷ ships).
    let slot2: ForestSlot = 7;
    for i in 0..n_floor {
        let v = vol.note_slot_ship(slot2, 1, SHIP_NS).await;
        let nf = plane.n_floor();
        if i + 1 >= nf {
            assert_eq!(
                v,
                ShipVerdict::OfferIdle { to: 1 },
                "ship {} ≥ N_floor {nf}",
                i + 1
            );
            break;
        }
        assert_eq!(v, ShipVerdict::Serve, "ship {} < N_floor {nf}", i + 1);
    }
    let AcquireSlotReply::Granted(g2) = vol.manager_acquire_slot(1, slot2).await.unwrap() else {
        panic!()
    };
    assert_eq!(g2.g, 2);
    let s = lease_stats(&vol);
    assert_eq!((s.handovers, s.offers), (2, 2), "two handovers");
    assert!(
        (2..1_000).contains(&plane.n_floor()),
        "N_floor after two handovers: {}",
        plane.n_floor()
    );
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// Review round 3 — the seq-space law's two consumers the round-2 pins
// missed: the §4.4 pt 4 rollback arms (Issue 20) and the record frontier
// (Issue 22), both on a ring whose stamps are OFFSET from its positions.
// ---------------------------------------------------------------------------

/// The Issue-2 shape as a fixture: region 1 (slot 4's busy holder) stamps
/// 400 entries, the slot is handed to region 0 — ring 0 now stamps
/// `position + seq_offset` with a NONZERO offset. Returns the routed set
/// and the (busy) ring 1 head.
async fn open_with_ring0_offset(uris: &[String], tag: u64, owner: u64) -> Arc<RoutedMetaBackend> {
    let routed = open_under(uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    for i in 0..400u64 {
        vol.commit_block_refs(owner, &refs(tag, owner, i, 1))
            .await
            .unwrap();
    }
    vol.manager_offer_slot(1, SLOT4, 0).await.unwrap();
    let AcquireSlotReply::Granted(_) = vol.manager_acquire_slot(0, SLOT4).await.unwrap() else {
        panic!()
    };
    assert!(
        vol.journal_ring().seq_offset() > 0,
        "the fixture: ring 0 stamps above its positions"
    );
    routed
}

/// **Issue 20 (round 3): the §4.4 pt 4 rollback arms address the overlay
/// by STAMPED seqs.** On a ring whose `seq_offset > 0` a member whose
/// apply fails (the poison seam fires AFTER the apply) must be rolled OUT
/// of RAM — its already-applied prefix removed by the seq range the
/// records actually carry — and a FAILED WRITE's whole-window rollback
/// must remove every member's records; RAM equals a fresh mount's replay
/// after both. Before the fix both arms addressed `[res.start, res.end())`
/// — the POSITION range — and removed nothing on such a ring.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollbacks_address_stamped_seqs_on_a_ring_with_an_offset() {
    use squeezefs::meta_backend::kv::backend::TEST_CONVEYOR_POISON_APPLY_INO;
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            TEST_CONVEYOR_POISON_APPLY_INO.store(0, Ordering::SeqCst);
            squeezefs::uring_fs::clear_faults();
        }
    }
    let _cleanup = Cleanup;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0000000000000020");
    let owner = ino_in_slot(SLOT4, 5);
    let routed = open_with_ring0_offset(&uris, tag, owner).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let offset = vol.journal_ring().seq_offset();

    // ---- Arm 1: a member whose apply fails is rolled out of RAM. ----
    let victim = routed
        .create(ROOT_INO, "poison_me", libc::S_IFREG | 0o600, 0, 0)
        .await
        .unwrap();
    let atime_before = routed.getattr(victim.ino).await.unwrap().atime;
    assert_ne!(atime_before, 1);
    // The seam names the LOCAL inode key (the routed layer encodes the
    // slot into the volume-local ino).
    let (_, local_victim) = routed.route_ino(victim.ino);
    TEST_CONVEYOR_POISON_APPLY_INO.store(local_victim, Ordering::SeqCst);
    let poisoned = routed
        .setattr(
            victim.ino,
            None,
            None,
            None,
            None,
            Some(1),
            Some(2),
            Some(3),
        )
        .await;
    TEST_CONVEYOR_POISON_APPLY_INO.store(0, Ordering::SeqCst);
    assert!(poisoned.is_err(), "the armed apply fault fails the member");
    assert_eq!(
        routed.getattr(victim.ino).await.unwrap().atime,
        atime_before,
        "the poisoned member's applied prefix is rolled OUT of RAM (ring 0 offset {offset})"
    );
    // The volume keeps working after the isolated failure.
    routed
        .create(ROOT_INO, "after_poison", libc::S_IFREG | 0o600, 0, 0)
        .await
        .unwrap();

    // ---- Arm 2: a failed WRITE's whole-window rollback. ----
    let ring0 = vol.journal_ring();
    let head = ring0.core().head();
    squeezefs::uring_fs::arm_sector_write_error(ring0.physical_offset_of(head));
    let failed = routed
        .create(ROOT_INO, "never_landed", libc::S_IFREG | 0o600, 0, 0)
        .await;
    squeezefs::uring_fs::clear_faults();
    assert!(failed.is_err(), "the armed ring-head fault fails the write");
    assert!(
        routed
            .lookup_dentry(ROOT_INO, "never_landed")
            .await
            .unwrap()
            .is_none(),
        "the failed write's records are rolled out of RAM (ring 0 offset {offset})"
    );
    routed
        .create(ROOT_INO, "after_fault", libc::S_IFREG | 0o600, 0, 0)
        .await
        .unwrap();
    assert_eq!(vol.block_ref_count(tag, 395).await.unwrap(), 1);

    // RAM == replay: a crash remount folds to the same digest, with the
    // rolled-back names absent and the survivors present.
    let ram = digest_backend(&vol).await.unwrap();
    drop(vol);
    drop(routed);
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(digest_backend(&vol).await.unwrap(), ram, "RAM == replay");
    assert!(routed
        .lookup_dentry(ROOT_INO, "never_landed")
        .await
        .unwrap()
        .is_none());
    assert!(routed
        .lookup_dentry(ROOT_INO, "after_fault")
        .await
        .unwrap()
        .is_some());
    let v = routed
        .lookup_dentry(ROOT_INO, "poison_me")
        .await
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(routed.getattr(v).await.unwrap().atime, atime_before);
    shutdown(&routed).await;
}

/// **Issue 22 (round 3): the record frontier is the RECEIVING ring's.**
/// After A → B with A's positions far ahead of B's (the fixture), B's
/// later release of the slot must clear its window within a cycle or two
/// — before the fix the per-slot frontier kept A's maximum, ring B's
/// `reusable_upto` could never reach it, and the release stormed 64
/// checkpoint cycles and aborted `Corrupt` (retried every tick).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_from_the_quieter_ring_clears_its_window_in_one_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0000000000000022");
    let owner = ino_in_slot(SLOT4, 5);
    let routed = open_with_ring0_offset(&uris, tag, owner).await;
    let vol = Arc::clone(&routed.volumes[0]);
    // Region 0 (the quieter ring) writes the slot once, then releases it.
    vol.commit_block_refs(owner, &refs(tag, owner, 500, 1))
        .await
        .unwrap();
    let ckpts_before = META_KV_CHECKPOINTS.load(Ordering::Relaxed);
    vol.release_slot_handover(0, SLOT4)
        .await
        .expect("the release lands — never a stuck-tail Corrupt");
    let cycles = META_KV_CHECKPOINTS.load(Ordering::Relaxed) - ckpts_before;
    assert!(
        cycles <= 3,
        "the flush cleared ring 0's window of the slot in {cycles} cycles (a stuck frontier \
         storms 64)"
    );
    assert!(!vol.slot_leases().unwrap().gate.is_leased(SLOT4));
    assert_eq!(vol.block_ref_count(tag, 500).await.unwrap(), 1);
    // The next open replays both rings without a Lease violation.
    drop(vol);
    drop(routed);
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(META_KV_REPLAY_LEASE_VIOLATIONS.load(Ordering::Relaxed), 0);
    assert_eq!(vol.block_ref_count(tag, 500).await.unwrap(), 1);
    shutdown(&routed).await;
}

/// **Issue 20 (round 3, the grep's third site): a §4.7 pending free is
/// gated on the freeing entry's POSITION, the tail's domain.** On a ring
/// stamping above its positions an SMO's retired extent must become
/// claimable once a covering cycle passes its entry — live, and again
/// when the free is REPLAYED from the window at the next open. Before
/// the fix the gate was the free record's STAMPED seq (`position +
/// offset`), so the extent stayed parked until the ring wrote `offset`
/// more bytes — on a quiet ring, for ever (`pending_free` never drains).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_smo_retirement_on_an_offset_ring_releases_on_its_entrys_coverage() {
    use squeezefs::meta_backend::kv::META_KV_PENDING_FREE_PARKED;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0000000000000021");
    let owner = ino_in_slot(SLOT4, 5);
    let routed = open_with_ring0_offset(&uris, tag, owner).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let offset = vol.journal_ring().seq_offset();
    // Enough native-slot records (→ ring 0) to split the root's xattr
    // leaf: the flush pass's SMO retires the predecessor image.
    let split_leaf = |routed: Arc<RoutedMetaBackend>, round: u32| async move {
        let value = vec![0x5Au8; 16 * 1024];
        for i in 0..24 {
            routed
                .setxattr(ROOT_INO, &format!("user.smo{round}_{i}"), &value)
                .await
                .unwrap();
        }
    };
    let parked0 = META_KV_PENDING_FREE_PARKED.load(Ordering::Relaxed);
    split_leaf(Arc::clone(&routed), 1).await;
    vol.checkpoint_now().await.unwrap();
    assert!(
        META_KV_PENDING_FREE_PARKED.load(Ordering::Relaxed) > parked0,
        "the fixture: an SMO retired an image on ring 0 (offset {offset})"
    );
    // Live arm: the covering cycles release the retirement (the FIND-VS-A
    // second cycle carries the tail past the split's dying floor).
    for _ in 0..4 {
        if vol.allocator().pending_count() == 0 {
            break;
        }
        vol.checkpoint_now().await.unwrap();
    }
    assert_eq!(
        vol.allocator().pending_count(),
        0,
        "a retirement on ring 0 (offset {offset}) drains on the entry's coverage"
    );
    // Replay arm: another SMO whose free is still IN THE WINDOW at a
    // crash; the next open re-parks it on the entry's position and the
    // first covering cycles drain it.
    split_leaf(Arc::clone(&routed), 2).await;
    vol.checkpoint_now().await.unwrap();
    assert!(
        vol.allocator().pending_count() > 0,
        "the fixture: the second SMO's free is parked in the window"
    );
    drop(vol);
    drop(routed);
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert!(
        vol.journal_ring().seq_offset() > 0,
        "the offset is recovered"
    );
    for _ in 0..4 {
        if vol.allocator().pending_count() == 0 {
            break;
        }
        vol.checkpoint_now().await.unwrap();
    }
    assert_eq!(
        vol.allocator().pending_count(),
        0,
        "a replayed free on an offset ring drains on its entry's coverage"
    );
    shutdown(&routed).await;
}

/// A stamped member of `len` bytes with an `ring` byte fixed ring — the
/// large-tree fixtures (a slot tree past the inline tails cap is ≈ 90 MiB
/// of 64 KiB leaves).
async fn format_stamped_member_sized(
    dir: &std::path::Path,
    name: &str,
    len: u64,
    ring: u64,
) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    let opts = FormatV3Options {
        journal_len_override: Some(ring),
        ..set_opts()
    };
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let r = format_v3_stamped(&p, len, &opts, plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format stamped member");
    p.display().to_string()
}

/// Grow slot `slot`'s tree past `nodes` extents (the slot's extent ledger
/// — leaves plus the few interior nodes) with 15 KiB xattr values (≈ 3
/// per 64 KiB leaf).
async fn grow_slot_tree(vol: &KvMetaBackend, slot: ForestSlot, nodes: usize) {
    let value = vec![0x3Cu8; 15 * 1024];
    let ledger = Arc::clone(&vol.slot_leases().unwrap().extents);
    let mut k = 0u64;
    while (ledger.get(slot) as usize) <= nodes {
        for _ in 0..64 {
            vol.setxattr_internal(ino_in_slot(slot, 1 + k / 8), &format!("user.t{k}"), &value)
                .await
                .unwrap();
            k += 1;
        }
        assert!(k < 200_000, "the tree never reached {nodes} extents");
    }
}

/// The reachable LEAF count of slot `slot`'s tree (a QUIET tree — the
/// walk is not serialized against SMOs).
async fn leaf_count(vol: &KvMetaBackend, slot: ForestSlot) -> usize {
    let tree = vol.slot_tree(slot).expect("the slot tree exists");
    let mut n = 0usize;
    for addr in tree.reachable_node_addrs().await.unwrap() {
        if vol
            .node_cache()
            .peek_tail_offset(addr)
            .await
            .unwrap()
            .is_some()
        {
            n += 1;
        }
    }
    n
}

/// **Issue 21 (round 3): a release records the tails of a tree PAST the
/// inline cap — spilled, complete, and the mount survives.** At 64 KiB
/// nodes the KV value cap carries ≈ 1,386 tail entries inline; a slot
/// tree of more leaves than that released to the manager must land (its
/// `slot_tails` record a SPILL locator, the images barriered before the
/// record named them), the next checkpoints must run (before the fix the
/// over-cap value passed `write_control_entry` and the flush pass wedged
/// on `ValueTooLarge` at tree 0's freeze — the clause-b FAIL-STOP class),
/// and a remount must read every leaf's tail back through the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_of_a_tree_past_the_inline_cap_spills_its_tails_and_the_mount_survives() {
    use squeezefs::meta_backend::kv::slot_state::{inline_tails_cap, SlotTails};
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![
        format_stamped_member_sized(dir.path(), "meta0", 320 * 1024 * 1024, 8 * 1024 * 1024).await,
    ];
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let cap = inline_tails_cap(NODE_SIZE / 4 + 256);
    grow_slot_tree(&vol, SLOT4, cap + 64).await;
    vol.release_slot_handover(1, SLOT4)
        .await
        .expect("the release lands whatever the tree's size");
    let leaves = leaf_count(&vol, SLOT4).await;
    assert!(
        leaves > cap,
        "the fixture: {leaves} leaves > the inline cap {cap}"
    );
    // The mount keeps cycling: the flush pass freezes tree 0's leaf
    // (before the fix: `ValueTooLarge` at the freeze, deferred node,
    // pinned tail, the clause-b FAIL-STOP).
    for _ in 0..3 {
        vol.checkpoint_now()
            .await
            .expect("the flush pass never wedges on tree 0");
    }
    assert!(!vol.is_failed(), "never the clause-b fail-stop");
    // The record is a SPILL locator, COMPLETE over the tree.
    let (g, tails) = vol
        .slot_tails(SLOT4)
        .await
        .unwrap()
        .expect("tails recorded");
    assert_eq!(g, 1);
    assert_eq!(tails.len(), leaves, "COMPLETE over the tree");
    let rec = vol.slot_tails_record(SLOT4).await.unwrap().unwrap();
    let SlotTails::Spilled(sp) = &rec.tails else {
        panic!("a set past the inline cap spills, got {:?}", rec.tails);
    };
    assert_eq!(sp.runs.len(), leaves.div_ceil(NODE_SIZE / 12));
    let heap = vol.superblock().heap.start;
    for (addr, _) in &sp.runs {
        assert!(
            vol.allocator()
                .is_allocated((addr - heap) / NODE_SIZE as u64),
            "the spill's extent at {addr:#x} is claimed in the bitmap"
        );
    }
    // A remount reads the same complete set through the record; the
    // slot is unleased and acquirable at g + 1.
    drop(vol);
    drop(routed);
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION_ALT)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert!(!vol.slot_leases().unwrap().gate.is_leased(SLOT4));
    let (g2, tails2) = vol.slot_tails(SLOT4).await.unwrap().expect("tails survive");
    assert_eq!((g2, tails2.len()), (1, leaves));
    assert_eq!(tails2, tails);
    let AcquireSlotReply::Granted(gr) = vol.manager_acquire_slot(0, SLOT4).await.unwrap() else {
        panic!()
    };
    assert_eq!(gr.g, 2);
    // The grant leaves the tails record alone (PR 5 reads generation 1's
    // tails while the slot is leased at 2).
    let (g3, tails3) = vol.slot_tails(SLOT4).await.unwrap().expect("kept");
    assert_eq!((g3, tails3.len()), (1, leaves));
    // The next release supersedes the record: generation 2's set spills
    // again and generation 1's spill extents are FREED by the superseding
    // entry (released to the pool, or already reused by the new spill —
    // lowest-free-first).
    vol.release_slot_handover(0, SLOT4).await.unwrap();
    let (g4, tails4) = vol.slot_tails(SLOT4).await.unwrap().expect("re-recorded");
    assert_eq!((g4, tails4.len()), (2, leaves));
    let rec2 = vol.slot_tails_record(SLOT4).await.unwrap().unwrap();
    let new_addrs = rec2.tails.spill_addrs();
    assert!(!new_addrs.is_empty(), "still past the inline cap");
    let heap = vol.superblock().heap.start;
    for (addr, _) in &sp.runs {
        assert!(
            new_addrs.contains(addr)
                || !vol
                    .allocator()
                    .is_allocated((addr - heap) / NODE_SIZE as u64),
            "generation 1's spill extent at {addr:#x} left the bitmap with its record"
        );
    }
    shutdown(&routed).await;
}

/// **Issue 21 (round 3): the leave's tree-0 batch is CHUNKED by the entry
/// cap.** The reviewer's arithmetic: a region holding 108 trees of 100
/// leaves releases 108 × (an `Unleased` put + a tails record of 100 inline
/// entries) ≈ 140 KiB of records — past `MAX_ENTRY_LEN` (128 KiB) as ONE
/// entry, the `EntryTooLarge` the first build hit AFTER its page said
/// `Releasing`. The packer splits it into entries that each fit, keeps
/// every member, keeps order, and charges each chunk's grant rewrite;
/// one member always goes.
#[test]
fn the_leaves_batch_of_108_hundred_leaf_trees_packs_into_entries_under_the_cap() {
    use squeezefs::meta_backend::kv::journal::{
        pack_entries, record_frame_len, ENTRY_HDR_LEN, MAX_ENTRY_LEN,
    };
    use squeezefs::meta_backend::kv::slot_state::{
        slot_state_key, slot_tails_key, SlotTails, SlotTailsRecord, UNLEASED_LEN,
    };
    let tails_len = |leaves: usize| {
        SlotTailsRecord {
            g: 1,
            tails: SlotTails::Inline(vec![(0x1000, 4096); leaves]),
        }
        .encode()
        .unwrap()
        .len()
    };
    let member = |leaves: usize| {
        record_frame_len(slot_state_key(1).len(), UNLEASED_LEN)
            + record_frame_len(slot_tails_key(1).len(), tails_len(leaves))
    };
    let payloads: Vec<u64> = (0..108).map(|_| member(100)).collect();
    let total: u64 = payloads.iter().sum();
    assert!(
        ENTRY_HDR_LEN + total > MAX_ENTRY_LEN,
        "the fixture: {total} B of records exceed one entry"
    );
    // A grant rewrite of ≤ 8 runs rides beside every chunk.
    let overhead = |_: std::ops::Range<usize>| record_frame_len(17, 3 + 8 * 12);
    let chunks = pack_entries(&payloads, overhead);
    assert!(chunks.len() >= 2, "{chunks:?}");
    assert_eq!(chunks.first().unwrap().start, 0);
    assert_eq!(chunks.last().unwrap().end, payloads.len());
    for w in chunks.windows(2) {
        assert_eq!(w[0].end, w[1].start, "contiguous, in order");
    }
    for c in &chunks {
        let len: u64 =
            ENTRY_HDR_LEN + payloads[c.clone()].iter().sum::<u64>() + overhead(c.clone());
        assert!(len <= MAX_ENTRY_LEN, "chunk {c:?} = {len} B fits one entry");
    }
    // A single member past the cap still goes alone (the write's refusal
    // is the loud outcome, never a silent split of one slot's records).
    let huge = vec![MAX_ENTRY_LEN, 10, 10];
    assert_eq!(pack_entries(&huge, |_| 0), vec![0..1, 1..3]);
    assert!(pack_entries(&[], |_| 0).is_empty());
}

/// **Issue 21 (round 3): the clean leave of a region holding several
/// trees records every tree's tails and the next open serves them.** A
/// declared region leasing three slots grows each to ≈ 120 leaves, leaves
/// cleanly, and the remount finds every slot `Unleased` at `g = 1` with a
/// COMPLETE inline tails record, the region's grant record free of the
/// released trees' images, the two-sided closure holding and C13 empty;
/// a first touch re-acquires each at `g = 2` and leaves the tails record
/// alone. (The multi-chunk shape at the real cap is ≈ 600 MiB of leaves
/// — the packer's own contract above pins that arithmetic exactly.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leave_of_a_region_holding_several_trees_records_every_trees_tails() {
    use squeezefs::meta_backend::kv::slot_state::SlotTails;
    let _ = env_logger::builder().is_test(true).try_init();
    const THREE: &str = "1:4,5,6";
    let slots: [ForestSlot; 3] = [4, 5, 6];
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![
        format_stamped_member_sized(dir.path(), "meta0", 128 * 1024 * 1024, 4 * 1024 * 1024).await,
    ];
    let closure = |s: &squeezefs::meta_backend::kv::appender::AppenderStats| {
        assert_eq!(
            s.grant_granted,
            s.grant_claimed + s.grant_returned + s.grant_unclaimed,
            "extent_grant_extents ≡ claimed + returned + unclaimed: {s:?}"
        );
    };
    let mut leaves = [0usize; 3];
    let images = {
        let routed = open_under(&uris, &Knobs::armed().partition(THREE)).await;
        let vol = Arc::clone(&routed.volumes[0]);
        for s in slots {
            grow_slot_tree(&vol, s, 120).await;
        }
        vol.checkpoint_now().await.unwrap();
        vol.checkpoint_now().await.unwrap();
        let heap = vol.superblock().heap.start;
        let node = u64::from(vol.superblock().node_size);
        let mut images: Vec<u64> = Vec::new();
        for (i, s) in slots.iter().enumerate() {
            leaves[i] = leaf_count(&vol, *s).await;
            assert!(leaves[i] >= 100, "slot {s}: {} leaves", leaves[i]);
            for a in vol
                .slot_tree(*s)
                .unwrap()
                .reachable_node_addrs()
                .await
                .unwrap()
            {
                images.push((a - heap) / node);
            }
        }
        closure(&vol.appender_stats().unwrap());
        // The clean leave: three `Unleased` puts + three tails records
        // (+ the grant rewrite) — one entry here, chunked past the cap.
        shutdown(&routed).await;
        images
    };
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION_ALT)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let states = tree0_states(&vol).await;
    for (i, s) in slots.iter().enumerate() {
        let (_, st) = states.iter().find(|(k, _)| k == s).expect("slot recorded");
        let SlotState::Unleased { g, .. } = st else {
            panic!("slot {s} is Unleased after the leave, got {st:?}");
        };
        assert_eq!(*g, 1);
        let (tg, tails) = vol.slot_tails(*s).await.unwrap().expect("tails recorded");
        assert_eq!(
            (tg, tails.len()),
            (1, leaves[i]),
            "slot {s}: COMPLETE tails"
        );
        let rec = vol.slot_tails_record(*s).await.unwrap().unwrap();
        assert!(
            matches!(rec.tails, SlotTails::Inline(_)),
            "≈ 120 leaves ride inline"
        );
    }
    let record = vol.extent_grant_record(1).await.unwrap();
    assert!(
        images.iter().all(|e| !record.contains(*e)),
        "the leave moved every released tree's images out of region 1's record"
    );
    closure(&vol.appender_stats().unwrap());
    assert!(vol.c13_orphan_image_extents().await.unwrap().is_empty());
    // First touches re-acquire at g = 2; the tails records are untouched.
    for (i, s) in slots.iter().enumerate() {
        let AcquireSlotReply::Granted(gr) = vol.manager_acquire_slot(0, *s).await.unwrap() else {
            panic!()
        };
        assert_eq!(gr.g, 2);
        let (tg, tails) = vol.slot_tails(*s).await.unwrap().unwrap();
        assert_eq!((tg, tails.len()), (1, leaves[i]));
    }
    shutdown(&routed).await;
}

/// **The reactive grant refill asks for the SMO's OWN need (round 3 —
/// found by the three-tree leave fixture).** A leased leaf whose overlay
/// splits into MORE parts than the one-SMO constant (`SMO_IMAGES_MAX` =
/// 4) with the region's grant drained must still compact: the refusal
/// names the need, the refill carves it, the retry lands. Before the fix
/// the refill asked for the constant, §5.3.5's idempotency answered the
/// 4-extent remainder VERBATIM at every later cycle while the split
/// needed 6, the compaction deferred for ever, the region's tail never
/// advanced and every commit parked at its full ring escalated through
/// D1.b (`commit aborted while parked for ring space`, 2 runs in 5).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leased_leafs_split_wider_than_the_one_smo_constant_is_refilled_to_its_need() {
    use squeezefs::meta_backend::kv::appender::SMO_IMAGES_MAX;
    use squeezefs::meta_backend::kv::META_KV_NODE_SPLITS;
    let _ = env_logger::builder().is_test(true).try_init();
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_SYM_GRANT_EXTENTS");
            std::env::remove_var("SQUEEZEFS_SYM_RING_KB");
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }
    let _cleanup = Cleanup;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    // The cadence's carve at its floor (8): the constant refill (4) plus
    // one cadence carve (8) stays below the split's need below. The
    // cadence PARKED and region 1's ring wide enough to hold the whole
    // overlay, so ONE flush meets the whole split.
    std::env::set_var("SQUEEZEFS_SYM_GRANT_EXTENTS", "8");
    std::env::set_var("SQUEEZEFS_SYM_RING_KB", "4096");
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    // The tree minted (its root is the grant's first claim), then region
    // 1's grant DRAINED: every unclaimed extent returned.
    let value = vec![0x77u8; 15 * 1024];
    let owner = ino_in_slot(SLOT4, 9);
    vol.setxattr_internal(owner, "user.mint", &value)
        .await
        .unwrap();
    let unclaimed = vol.region_grant_unclaimed(1);
    assert!(!unclaimed.is_empty(), "the join minted a grant");
    vol.manager_return_extents(1, &unclaimed).await.unwrap();
    assert!(vol.region_grant_unclaimed(1).is_empty());
    // One leaf's overlay: 48 × 15 KiB values under one ino — a split into
    // ≥ 14 parts of a 64 KiB node, more than the constant plus one
    // cadence carve at the floor cover.
    for k in 0..48 {
        vol.setxattr_internal(owner, &format!("user.wide{k}"), &value)
            .await
            .unwrap();
    }
    let splits0 = META_KV_NODE_SPLITS.load(Ordering::Relaxed);
    let stalls0 = vol.appender_stats().unwrap().regions[1].dependency_stalls;
    for _ in 0..3 {
        vol.checkpoint_now().await.unwrap();
    }
    assert!(
        META_KV_NODE_SPLITS.load(Ordering::Relaxed) > splits0,
        "the wide split ran (the refill covered its need)"
    );
    let stalls = vol.appender_stats().unwrap().regions[1].dependency_stalls - stalls0;
    assert!(
        stalls <= 1,
        "at most the one refusal that named the need, never a stall per cycle: {stalls}"
    );
    let grant = vol.appender_stats().unwrap().regions[1].grant_claimed;
    assert!(
        grant > u64::from(SMO_IMAGES_MAX),
        "the split's images exceed the one-SMO constant: {grant} claimed"
    );
    assert!(!vol.is_failed());
    shutdown(&routed).await;
}

/// **Issue 25 (round 4): an UNLEASED slot tree's structure is the
/// MANAGER's to maintain; a tree another appender leases is SKIPPED,
/// never refused.** Region 1 fills slot 4 with 12 KiB payloads, deletes
/// nine in ten (underfull leaf pairs), releases the slot; the manager's
/// merge sweep (the D4 arm's and the heap-full recovery's shared body)
/// must MERGE the unleased tree — before the fix the third gate state
/// refused the manager's structural moves as `NotLeased`, the must-stay-0
/// `meta_kv_leaf_lease_refusals` counted, the sweep aborted its lap at
/// that tree and the tick errored every cadence. Then a WIRE joiner takes
/// the slot: the manager's sweep skips its tree (`merge_sweep_foreign_
/// skips`), completes the lap, and refuses nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_manager_merges_an_unleased_slot_tree_and_skips_a_foreign_leased_one() {
    use squeezefs::meta_backend::kv::META_KV_NODE_MERGES;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let refusals0 = META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed);
    // ≈ 10 leaves of 12 KiB payloads in slot 4; keep every tenth.
    let value = vec![0x42u8; 12 * 1024];
    for k in 0..40u64 {
        vol.setxattr_internal(ino_in_slot(SLOT4, 1 + k), "user.payload", &value)
            .await
            .unwrap();
    }
    vol.checkpoint_now().await.unwrap();
    for k in 0..40u64 {
        if k % 10 != 0 {
            vol.removexattr_internal(ino_in_slot(SLOT4, 1 + k), "user.payload")
                .await
                .unwrap();
        }
    }
    vol.checkpoint_now().await.unwrap();
    vol.checkpoint_now().await.unwrap();
    let census = vol.dead_bset_census();
    assert!(
        census.merge_candidates.len() >= 2,
        "the fixture: underfull leaves in slot 4 ({} of {})",
        census.merge_candidates.len(),
        census.leaves
    );
    // Region 1 releases the slot: unleased, the manager's to maintain.
    vol.release_slot_handover(1, SLOT4).await.unwrap();
    assert!(!vol.slot_leases().unwrap().gate.is_leased(SLOT4));
    let merges0 = META_KV_NODE_MERGES.load(Ordering::Relaxed);
    let report = vol
        .defrag_merge_sweep(None)
        .await
        .expect("the manager's sweep runs over an unleased slot tree");
    assert!(report.lap_complete, "the lap completes");
    assert!(
        META_KV_NODE_MERGES.load(Ordering::Relaxed) > merges0,
        "the unleased tree's underfull pair merged"
    );
    assert_eq!(
        META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed),
        refusals0,
        "the manager's maintenance of an unleased tree is never a lease refusal"
    );
    // The survivors read back.
    for k in (0..40u64).step_by(10) {
        let v = vol
            .getxattr(ino_in_slot(SLOT4, 1 + k), "user.payload")
            .await
            .unwrap();
        assert_eq!(v.as_deref(), Some(value.as_slice()));
    }
    // A WIRE joiner takes slot 4 (routing 3): its tree is FOREIGN to this
    // mount's structure — the sweep skips it, completes, refuses nothing.
    let (host, mut client, joiner) = wire_joiner(&vol, 25).await;
    match client.acquire_slot(joiner, 3).await.unwrap() {
        ManagerReply::SlotsGranted { slots, .. } => assert_eq!(slots[0].slot, 3),
        other => panic!("{other:?}"),
    }
    // The joiner's id is never a released in-process region's (page 1
    // went `Free` with region 1's release — its id stays the region
    // object's; a joiner handed it would be routed as that region).
    assert!(
        joiner >= 2,
        "a wire joiner never reuses an in-process region's id: {joiner}"
    );
    assert!(vol.slot_leases().unwrap().gate.is_foreign(SLOT4));
    let skips0 = lease_stats(&vol).merge_sweep_foreign_skips;
    let report = vol.defrag_merge_sweep(None).await.unwrap();
    assert!(report.lap_complete, "a foreign tree never blocks the lap");
    assert!(
        lease_stats(&vol).merge_sweep_foreign_skips > skips0,
        "the foreign-leased tree was skipped, counted"
    );
    assert_eq!(
        META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed),
        refusals0,
        "skipped, never refused"
    );
    assert!(!vol.is_failed());
    host.shutdown();
    shutdown(&routed).await;
}

/// **Issue 24 (round 3): a first-touch acquire never parks for ring space
/// under `manager_verbs`.** With the cadence parked and ring 0's user
/// window exhausted, a first-touch commit's door acquire PARKS at ring
/// admission; the checkpoint task's grant verb (the cadence refill, the
/// flush pass's reactive refill) must still get the verb mutex — before
/// the fix the acquire parked HOLDING it, the cycle that would have freed
/// the ring blocked behind it, and the D1.b escalation was the only exit.
/// After the fix the grant verb answers within the bound, a checkpoint
/// frees the ring and every parked commit — the first touch included —
/// lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_touch_acquire_parked_for_ring_space_never_holds_the_verb_mutex() {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::env::remove_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS");
        }
    }
    let _cleanup = Cleanup;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    // The cadence parked: nothing but the test advances the ring's tail.
    std::env::set_var("SQUEEZEFS_META_FLUSH_INTERVAL_MS", "60000");
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    // Fill ring 0's USER window: 16 KiB xattr values on the root (native
    // slot → ring 0) until a committer PARKS at admission.
    let value = vec![0xA5u8; 16 * 1024];
    let stalls0 = vol.journal_full_stalls();
    let mut fillers = Vec::new();
    let mut n = 0;
    while vol.journal_full_stalls() == stalls0 {
        n += 1;
        assert!(n < 400, "the ring never filled");
        let routed = Arc::clone(&routed);
        let value = value.clone();
        let name = format!("user.fill{n}");
        fillers.push(tokio::spawn(async move {
            routed.setxattr(ROOT_INO, &name, &value).await
        }));
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        if n > 2 && vol.journal_full_stalls() == stalls0 {
            // Give the conveyor a moment to admit or stall the batch.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    // The parked batch leaves the user window ALMOST full — the acquire's
    // control entry is a few hundred bytes: take the remainder as held
    // (never reserved) admissions so even that entry cannot be admitted.
    let ring0 = vol.journal_ring();
    let mut held = Vec::new();
    while let Some(adm) = ring0.try_admit(
        64,
        squeezefs::meta_backend::kv::journal_core::AdmissionClass::User,
    ) {
        held.push(adm);
        assert!(held.len() < 1 << 16, "the user window never exhausted");
    }
    let stalls1 = vol.journal_full_stalls();
    // A first touch of a slot with no lease: its acquire needs a ring-0
    // control entry — it must PARK for space (a user commit's law)…
    let tag = volume_tag("vol-0000000000000024");
    let fresh: ForestSlot = 300;
    let owner = ino_in_slot(fresh, 7);
    let first_touch = {
        let vol = Arc::clone(&vol);
        tokio::spawn(async move { vol.commit_block_refs(owner, &refs(tag, owner, 1, 1)).await })
    };
    while vol.journal_full_stalls() == stalls1 {
        assert!(
            !first_touch.is_finished(),
            "the first touch cannot land on a full ring"
        );
        tokio::task::yield_now().await;
    }
    // … and while it parks, the checkpoint task's GRANT VERB (driven here
    // as the task would) must still be served: the verb mutex is free.
    let grant = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        vol.manager_extent_grant(1, 8),
    )
    .await;
    assert!(
        grant.is_ok(),
        "the grant verb blocked behind a parked first-touch acquire — the verb mutex was held \
         across a ring-space park (the lock-order inversion the D1.b fail-stop resolves)"
    );
    // The test plays the tick: the held budget returns, one covering cycle
    // frees the ring; every parked commit — the first touch included —
    // lands.
    // The test plays the tick: the held budget returns, and covering
    // cycles free the ring (the FIND-VS-A law — a split's dying floor
    // clamps the cycle that retires it, the next cycle passes it — so
    // the tick's continuous cadence is played as a bounded loop); every
    // parked commit — the first touch included — lands.
    for adm in held {
        ring0.core().release(adm);
    }
    for _ in 0..8 {
        vol.checkpoint_now().await.unwrap();
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        if fillers.iter().all(|f| f.is_finished()) && first_touch.is_finished() {
            break;
        }
    }
    let settled = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for f in fillers {
            f.await.unwrap().unwrap();
        }
        first_touch.await.unwrap().unwrap();
    })
    .await;
    assert!(
        settled.is_ok(),
        "every parked commit lands once the ring is freed — the D1.b threshold never binds"
    );
    assert!(vol.slot_leases().unwrap().gate.is_leased(fresh));
    assert_eq!(vol.block_ref_count(tag, 1).await.unwrap(), 1);
    assert!(!vol.is_failed(), "never the D1.b fail-stop");
    shutdown(&routed).await;
}

/// **Issue 23 (round 3): the rotor cap survives a crash-remount.** A
/// solo mount's 64 rotor grants are re-adopted at the next open from
/// tree 0, which does not carry the rotor bit; the arm rebuilds the rotor
/// AND re-marks its slots, so `rotor_held_by(0)` reads 64 again and a
/// rotor ask past `2 × M` refuses `RotorAtCap` exactly as on the first
/// mount — before the fix the re-adopted rotor read as 0 held and the
/// cap admitted ≈ 3M.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_rotor_cap_counts_the_re_adopted_rotor_after_a_crash_remount() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let vol = Arc::clone(&routed.volumes[0]);
        let plane = Arc::clone(vol.slot_leases().unwrap());
        assert_eq!(plane.table.rotor_held_by(0), MINT_SPREAD as u64);
        vol.checkpoint_now().await.unwrap();
        // A crash: no leave.
        drop(vol);
        drop(routed);
    }
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let plane = Arc::clone(vol.slot_leases().unwrap());
    let m = plane.mint_slots();
    assert_eq!(
        lease_stats(&vol).rotor,
        MINT_SPREAD as u64,
        "the rotor is rebuilt"
    );
    assert_eq!(
        plane.table.rotor_held_by(0),
        MINT_SPREAD as u64,
        "the re-adopted rotor counts against the cap"
    );
    // A rotor ask for M more lands (2M = the cap), the next refuses.
    let more = vol
        .manager_acquire_slots(0, m as u16, &[], ControlAdmit::Try)
        .await
        .unwrap();
    assert_eq!(more.len(), m as usize);
    assert_eq!(plane.table.rotor_held_by(0), 2 * m);
    let e = vol
        .manager_acquire_slots(0, 1, &[], ControlAdmit::Try)
        .await
        .expect_err("past 2 × M the rotor ask refuses");
    assert!(
        matches!(e, KvError::RotorAtCap { held, cap, .. } if held == 2 * m && cap == 2 * m),
        "{e:?}"
    );
    shutdown(&routed).await;
}

/// **Issue 23 (round 3) + Issue 26 (round 4): the appender page carries a
/// LAYOUT version and a page of another layout is REFUSED — by the
/// decoder, by the four-slot reader and by the OPEN — never read as
/// absent.** A page of this binary's layout decodes; the same image with
/// the layout byte at its pre-PR-4 value (0 — the slots then sat at 306
/// with no `seq_offset` word) and a RE-STAMPED checksum classifies
/// `ForeignLayout { 0 }` (not `Corrupt`: a torn page falls back to a
/// predecessor, a foreign one must not), `newest_valid` over the four
/// slots refuses with the forward-only message even beside a newer valid
/// page, and a volume whose page 0 was written by that layout refuses to
/// OPEN naming the reformat — the round-3 reader dropped it and tried the
/// next slot, reading a whole region as never-joined (`manager_lease`
/// `vacant`, no `seq_offset`, the page-only live roots gone). Also: a
/// `Free` page written at the leave carries the ring's offset IN FORCE,
/// and a door token returned with none out leaves the count at 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_page_layout_is_versioned_and_a_foreign_layout_refuses_the_open() {
    use squeezefs::meta_backend::kv::appender::{
        classify_page, newest_valid, page_checksum, AppenderPage, PageRead,
        APPENDER_PAGE_LAYOUT_VERSION,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-0000000000000023");
    let owner = ino_in_slot(SLOT4, 5);
    let routed = open_with_ring0_offset(&uris, tag, owner).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let offset = vol.journal_ring().seq_offset();
    assert!(offset > 0);
    // The offset was raised by the grant AFTER the last checkpoint's page
    // write; the clean leave's `Free` page must carry it.
    let path = std::path::Path::new(&uris[0]);
    let before = read_directory(path, vol.superblock()).await.unwrap();
    let p0 = before[0].page.clone().unwrap();
    assert_eq!(p0.state, AppenderState::Live);
    let sb = vol.superblock().clone();
    shutdown(&routed).await;
    let after = read_directory(path, &sb).await.unwrap();
    let p0 = after[0].page.clone().unwrap();
    assert_eq!(p0.state, AppenderState::Free);
    assert_eq!(
        p0.seq_offset, offset,
        "the Free page names the offset in force"
    );
    // The layout byte: this binary's value decodes; the pre-PR-4 value
    // with a valid checksum is its OWN class and refuses the read.
    let img = p0.encode().unwrap();
    assert_eq!(img[55], APPENDER_PAGE_LAYOUT_VERSION);
    assert!(matches!(classify_page(&img), PageRead::Valid(_)));
    let mut old = img.clone();
    old[55] = 0;
    let sum = page_checksum(&old);
    old[16..24].copy_from_slice(&sum.to_le_bytes());
    let e = AppenderPage::decode(&old).expect_err("the PR 2/3 layout refuses");
    assert!(
        e.to_string().contains("layout version 0"),
        "names the layout law: {e}"
    );
    assert_eq!(classify_page(&old), PageRead::ForeignLayout { version: 0 });
    let e = newest_valid(&[img.clone(), old.clone()]).expect_err("the reader refuses");
    assert!(
        e.to_string().contains("forward-only") && e.to_string().contains("reformat"),
        "{e}"
    );
    // The OPEN: page 0's NEWEST slot re-stamped to the foreign layout —
    // the mount refuses naming the layout, never opens over an absent
    // manager page.
    let offs0 = squeezefs::meta_backend::kv::appender::appender0_page_offsets(&sb.journal);
    let newest_slot = {
        let mut imgs = Vec::new();
        for off in &offs0 {
            imgs.push(
                squeezefs::uring_fs::read_at(path, *off, 4096)
                    .await
                    .unwrap(),
            );
        }
        newest_valid(&imgs).unwrap().expect("page 0").0
    };
    let mut foreign = squeezefs::uring_fs::read_at(path, offs0[newest_slot], 4096)
        .await
        .unwrap()
        .to_vec();
    foreign[55] = 0;
    let sum = page_checksum(&foreign);
    foreign[16..24].copy_from_slice(&sum.to_le_bytes());
    squeezefs::uring_fs::write_at(path, offs0[newest_slot], foreign)
        .await
        .unwrap();
    Knobs::armed().apply();
    let refused = open_routed_meta_set(&uris).await;
    Knobs::clear();
    let msg = match refused {
        Ok(_) => panic!("a foreign-layout manager page must refuse the open"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("layout version 0") && msg.contains("reformat"),
        "the open names the layout law: {msg}"
    );
    // The door: a leave with no token out stays at 0.
    let gate = squeezefs::slot_lease_core::LeaseGate::new();
    assert_eq!(gate.leave(3), 0);
    assert_eq!(gate.inflight(3), 0);
    drop(vol);
    drop(routed);
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
            .expect_err("the seam kills the holder");
        TEST_HANDOVER_HOLD_AFTER_PAGE.store(false, Ordering::Relaxed);
        assert!(
            e.to_string().contains("TEST_HANDOVER_HOLD_AFTER_PAGE"),
            "{e}"
        );
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
        assert_ne!(
            se.root.addr, 0,
            "the flushed tree's root travels on the page"
        );
        let states = tree0_states(&vol).await;
        assert!(matches!(
            states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st),
            Some(SlotState::Leased {
                appender_id: 1,
                g: 1,
                ..
            })
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
        assert_eq!(
            vol.block_ref_count(tag, i * 10).await.unwrap(),
            1,
            "block {i}"
        );
    }
    assert_eq!(digest_backend(&vol).await.unwrap(), digest_before);
    // A fresh acquire takes the tree at g + 1 with the page's root.
    let g = vol
        .manager_acquire_slots(0, 0, &[SLOT4], ControlAdmit::Try)
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
            .expect_err("the seam kills the holder");
        TEST_HANDOVER_HOLD_AFTER_TREE0.store(false, Ordering::Relaxed);
        assert!(
            e.to_string().contains("TEST_HANDOVER_HOLD_AFTER_TREE0"),
            "{e}"
        );
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
        .manager_acquire_slots(1, 0, &[SLOT4], ControlAdmit::Try)
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
        .expect_err("the seam kills the holder");
    TEST_HANDOVER_HOLD_AFTER_TREE0.store(false, Ordering::Relaxed);
    let g = vol
        .manager_acquire_slots(0, 0, &[SLOT4], ControlAdmit::Try)
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
        Some(SlotState::Leased {
            appender_id: 0,
            g: 3,
            ..
        })
    ));
    let plane = vol.slot_leases().expect("armed");
    assert!(plane.gate.is_leased(SLOT4) && !plane.gate.is_releasing(SLOT4));
    assert_eq!(
        vol.appender_stats().unwrap().regions[1].leases,
        1,
        "the alternate slot only"
    );
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
    assert!(p0.slots.iter().any(|e| guest_forest_slot(e.slot) == SLOT4
        && e.g == 3
        && e.state == SlotEntryState::Live));
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
            vol.note_slot_ship(slot, joiner, SHIP_NS).await;
        }
        assert_eq!(lease_stats(&vol).offers, 1);
        // The accept dies with the manager after tree 0 wrote the release.
        TEST_HANDOVER_HOLD_AFTER_TREE0.store(true, Ordering::Relaxed);
        let r = client.acquire_slot(joiner, routing).await;
        TEST_HANDOVER_HOLD_AFTER_TREE0.store(false, Ordering::Relaxed);
        assert!(
            r.is_err() || !matches!(r, Ok(ManagerReply::SlotsGranted { .. })),
            "{r:?}"
        );
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
    let mut client = ManagerClient::connect(&host.endpoint().to_string(), SECRET, "joiner-3", 0)
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
            assert!(
                !already,
                "the release landed, the grant had not: a fresh grant"
            );
            assert_eq!(slots.len(), 1);
            assert_eq!(slots[0].g, 2);
            assert_ne!(slots[0].words.root, (0, 0), "the flushed tree travels");
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
        .expect_err("a foreign slot's mutation refuses");
    assert!(e.to_string().contains(&format!("appender {joiner}")), "{e}");
    let plane = vol.slot_leases().unwrap();
    assert_eq!(
        plane.holders.holder(slot).map(|h| (h.appender_id, h.g)),
        Some((joiner, g))
    );
    let mut client =
        ManagerClient::connect(&host.endpoint().to_string(), SECRET, "joiner-3-again", 0)
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
        // Bit-12 ino LANES are retired for metadata under bit 17: the
        // mint is the SLOT's cursor, never a lane cursor — the default
        // format stamps bit 12 (the rung-10b flip), and an armed mount
        // still constructs no lane partition, so no lane cell exists.
        assert!(vol
            .ino_lane_snapshot(
                squeezefs::meta_backend::kv::ino_lane::InoSpace::Guest(routing),
                squeezefs::meta_backend::kv::journal::AppendPartition::SOLO,
            )
            .is_none());
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
        assert_eq!(
            vol.guest_cursor_snapshot(routing),
            None,
            "the cursor left with the lease"
        );
        let states = tree0_states(&vol).await;
        match states.iter().find(|(s, _)| *s == slot).map(|(_, st)| st) {
            Some(SlotState::Unleased { cursor, .. }) => assert_eq!(*cursor, top + 1),
            other => panic!("{other:?}"),
        }
        // Re-acquire in-process: the next mint is above the top.
        let g = vol
            .manager_acquire_slots(0, 0, &[slot], ControlAdmit::Try)
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
        .manager_acquire_slots(0, 0, &[slot], ControlAdmit::Try)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        g.words.cursor,
        minted + 1,
        "the cursor after the last mint travelled"
    );
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
    assert_eq!(
        (s.rotor, s.forced_shrinks),
        (64, 32),
        "M in force is 64; the rotor holds 32"
    );
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
        let AcquireSlotReply::Granted(_) = vol.manager_acquire_slot(0, SLOT4).await.unwrap() else {
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
        assert_eq!(
            vol.appender_stats().unwrap().live,
            1,
            "region 0 alone is joined"
        );
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
        .find(|e| {
            e.appender_id != 0
                && e.page
                    .as_ref()
                    .is_some_and(|p| p.slots.iter().any(|s| s.slot == 300))
        })
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

// ---------------------------------------------------------------------------
// §5.9 — the membership carriage.
// ---------------------------------------------------------------------------

/// The renewal `Grant` carries a member's slot leases (`slot_leases_ack`
/// — the manager's attestation, `(routing slot, g)` under the member's
/// epoch), the manager's recalls (`slot_release_notices` — a requester's
/// accepted offer of a slot a WIRE holder holds rides the holder's
/// renewal, since a member serves no push channel) and the offers
/// standing for it (`offered_slots`); the recall clears with the
/// holder's `ReleaseSlot`, after which the requester's retry is granted.
/// A member no page names, and every mount without an armed plane, gets
/// an empty carriage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_renewal_grant_carries_the_members_slot_leases_recalls_and_offers() {
    use squeezefs::membership::{
        JoinOutcome, JoinRequest, LeaseClock, LeaseClocks, MemberRole, MembershipOwner,
        RenewOutcome,
    };
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let (host, mut client, joiner) = wire_joiner(&vol, 6).await;
    let ident = joiner_identity(6);
    let member_id = squeezefs::cowriter::node_member_id_of(ident.node_token, ident.mount_slot);
    assert_eq!(
        squeezefs::cowriter::parse_node_member_id(&member_id),
        Some((ident.node_token, ident.mount_slot))
    );
    // The joiner leases routing slot 400.
    let routing: u16 = 400;
    let fslot = guest_forest_slot(routing);
    match client.acquire_slot(joiner, routing).await.unwrap() {
        ManagerReply::SlotsGranted { .. } => {}
        other => panic!("{other:?}"),
    }
    // The membership owner (the S6 plane, in-process): the joiner's join
    // grant attests its lease.
    let owner = MembershipOwner::arm(
        "owner-pr4",
        1,
        0,
        LeaseClocks::derive(std::time::Duration::from_micros(250)).unwrap(),
        LeaseClock::monotonic(),
    )
    .unwrap();
    let req = |id: &str| JoinRequest {
        id: id.to_string(),
        role: MemberRole::Writer,
        endpoint: None,
        pid: std::process::id(),
        boot: "pr4-carriage".to_string(),
        prior_epoch: None,
        pr_key: 0,
        mount: None,
    };
    let JoinOutcome::Granted(grant) = owner.join(req(&member_id)) else {
        panic!("join refused");
    };
    assert_eq!(grant.slot_leases_ack.slots, vec![(routing, 1)]);
    assert_eq!(grant.slot_leases_ack.prior_epoch, 0);
    assert!(grant.slot_release_notices.is_empty() && grant.offered_slots.is_empty());
    let epoch = grant.epoch;
    // A member no page names: an empty carriage.
    let JoinOutcome::Granted(other) = owner.join(req("node_00000000deadbeef.m00000001")) else {
        panic!("join refused");
    };
    assert!(other.slot_leases_ack.slots.is_empty());
    // An OFFER for the joiner: the manager's idle rotor slot 7 (routing
    // 6), touched by the joiner N_floor times.
    let plane = vol.slot_leases().expect("armed");
    let offered_slot: ForestSlot = 7;
    for _ in 0..plane.n_floor() {
        vol.note_slot_ship(offered_slot, joiner, SHIP_NS).await;
    }
    let RenewOutcome::Renewed(grant) = owner.renew(&member_id, epoch, 0) else {
        panic!("renew refused");
    };
    assert_eq!(grant.offered_slots, vec![(6u16, 1u32)]);
    assert_eq!(grant.slot_leases_ack.slots, vec![(routing, 1)]);
    assert_eq!(grant.slot_leases_ack.prior_epoch, epoch);
    // A RECALL: the joiner (the HOLDER decides — its own dominance
    // evaluation runs at its mount) offers slot 400 to the manager, which
    // accepts — the wire holder's recall rides the carriage; the requester
    // is answered "retry after the release".
    client.offer_slot(joiner, routing, 0).await.unwrap();
    match vol.manager_acquire_slot(0, fslot).await.unwrap() {
        AcquireSlotReply::Refused { holder, g } => assert_eq!((holder, g), (joiner, 1)),
        other => panic!("{other:?}"),
    }
    assert_eq!(lease_stats(&vol).recall_notices, 1);
    assert_eq!(vol.appender_stats().unwrap().manager_verb_refusals, 0);
    let RenewOutcome::Renewed(grant) = owner.renew(&member_id, epoch, 0) else {
        panic!("renew refused");
    };
    assert_eq!(grant.slot_release_notices, vec![routing]);
    // The holder releases (its flush-then-transfer is its own; the tree
    // was never minted, so the words are empty), the recall clears, the
    // requester's retry is granted at g + 1.
    let already = client
        .release_slot(
            joiner,
            routing,
            1,
            squeezefs::meta_ship::manager::WireSlotWords::default(),
            Vec::new(),
        )
        .await
        .unwrap();
    assert!(!already, "the release landed once");
    let RenewOutcome::Renewed(grant) = owner.renew(&member_id, epoch, 0) else {
        panic!("renew refused");
    };
    assert!(grant.slot_release_notices.is_empty());
    assert!(
        grant.slot_leases_ack.slots.is_empty(),
        "the lease left with the release"
    );
    match vol.manager_acquire_slot(0, fslot).await.unwrap() {
        AcquireSlotReply::Granted(g) => assert_eq!(g.g, 2),
        other => panic!("{other:?}"),
    }
    host.shutdown();
    shutdown(&routed).await;
    // The leave uninstalled the carriage source: nothing answers.
    assert!(
        squeezefs::membership::slot_lease_carriage_for_member(&member_id)
            .leases
            .is_empty()
    );
}

/// The mount path's venue (deliverable 4 — PR 3's owed wiring): the
/// manager verbs ride the S8 owner listener's `AsyncVerbRouter` through
/// `ManagerSetService`, dispatched by the frame's volume ordinal; a frame
/// naming an ordinal the set does not have is refused naming the width.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_manager_verbs_ride_the_owner_listener_dispatched_by_volume_ordinal() {
    use squeezefs::data_grant::AsyncVerbRouter;
    use squeezefs::meta_ship::manager::ManagerSetService;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let set_svc = ManagerSetService::new(&routed.volumes);
    assert_eq!(set_svc.width(), 1);
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        Arc::new(AsyncVerbRouter::new().with_manager(set_svc)),
    )
    .unwrap();
    let endpoint = host.endpoint().to_string();
    let mut client = ManagerClient::connect(&endpoint, SECRET, "joiner-9", 0)
        .await
        .unwrap();
    let reply = client.join(joiner_identity(9), 0).await.unwrap();
    let ManagerReply::Joined { appender_id, .. } = reply else {
        panic!("{reply:?}");
    };
    match client.acquire_slot(appender_id, 500).await.unwrap() {
        ManagerReply::SlotsGranted { slots, .. } => assert_eq!(slots[0].slot, 500),
        other => panic!("{other:?}"),
    }
    let mut wrong = ManagerClient::connect(&endpoint, SECRET, "joiner-9b", 5)
        .await
        .unwrap();
    let e = wrong
        .resolve_slot(500)
        .await
        .expect_err("an ordinal past the set refuses");
    let msg = e.to_string();
    assert!(
        msg.contains("ordinal 5") && msg.contains("1 volume"),
        "{msg}"
    );
    host.shutdown();
    shutdown(&routed).await;
}

/// **Issue 13's owed pin (round 3, Issue 23): two armed volumes, a wire
/// joiner on each.** The S4 owner table is composed per VOLUME: joiner A
/// leases routing slot 300 on volume 0 and joiner B routing slot 400 on
/// volume 1 through ONE `ManagerSetService` listener addressed by
/// ordinal; both slots read FOREIGN and every other slot local (the
/// second install never re-marks the first volume's foreign slot local),
/// each volume's tree 0 names its own lessee only, a release on one
/// volume withdraws that slot alone, and the whole set's leave reads
/// `solo`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_armed_volumes_each_with_a_wire_joiner_compose_one_owner_table() {
    use squeezefs::data_grant::AsyncVerbRouter;
    use squeezefs::meta_ship::manager::ManagerSetService;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    // Two stamped members of one set (one plan, two stamps).
    let plan = plan_meta_slot_set(2).expect("derived plan");
    let mut uris = Vec::new();
    for (i, name) in ["meta0", "meta1"].iter().enumerate() {
        let p = dir.path().join(name);
        std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
        std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
        let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[i].clone()).await;
        std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
        r.expect("format stamped member");
        uris.push(p.display().to_string());
    }
    let routed = open_under(&uris, &Knobs::armed()).await;
    assert_eq!(routed.volumes.len(), 2);
    assert!(routed.volumes.iter().all(|v| v.slot_lease_armed()));
    let set_svc = ManagerSetService::new(&routed.volumes);
    assert_eq!(set_svc.width(), 2);
    let host = cw::RpcListener::start_async(
        listener_cfg(),
        SECRET.to_vec(),
        Arc::new(AsyncVerbRouter::new().with_manager(set_svc)),
    )
    .unwrap();
    let endpoint = host.endpoint().to_string();
    let (slot_a, slot_b): (u16, u16) = (300, 400);
    let mut clients = Vec::new();
    for (ordinal, slot) in [(0u16, slot_a), (1u16, slot_b)] {
        let mut client =
            ManagerClient::connect(&endpoint, SECRET, &format!("joiner-v{ordinal}"), ordinal)
                .await
                .unwrap();
        let reply = client
            .join(joiner_identity(20 + u64::from(ordinal)), 0)
            .await
            .unwrap();
        let ManagerReply::Joined { appender_id, .. } = reply else {
            panic!("{reply:?}");
        };
        match client.acquire_slot(appender_id, slot).await.unwrap() {
            ManagerReply::SlotsGranted { slots, already } => {
                assert!(!already);
                assert_eq!((slots[0].slot, slots[0].g), (slot, 1));
            }
            other => panic!("{other:?}"),
        }
        clients.push((client, appender_id));
    }
    // The composed table: both foreign, everything else local.
    assert_eq!(squeezefs::dlm_slot::dlm_mode(), "slot-homed");
    assert!(!squeezefs::dlm_slot::is_local_slot(u64::from(slot_a)));
    assert!(!squeezefs::dlm_slot::is_local_slot(u64::from(slot_b)));
    assert!(squeezefs::dlm_slot::is_local_slot(1));
    assert!(squeezefs::dlm_slot::is_local_slot(500));
    // Each volume's tree 0 names ITS lessee only.
    for (ordinal, (mine, other)) in [(0usize, (slot_a, slot_b)), (1usize, (slot_b, slot_a))] {
        let states = tree0_states(&routed.volumes[ordinal]).await;
        let (_, joiner) = clients[ordinal];
        assert!(
            matches!(
                states
                    .iter()
                    .find(|(s, _)| *s == guest_forest_slot(mine))
                    .map(|(_, st)| st),
                Some(SlotState::Leased { appender_id, g: 1, .. }) if *appender_id == joiner
            ),
            "volume {ordinal} leases {mine} to its joiner"
        );
        assert!(
            !states
                .iter()
                .any(|(s, st)| *s == guest_forest_slot(other)
                    && matches!(st, SlotState::Leased { .. })),
            "volume {ordinal} knows nothing of the other volume's slot {other}"
        );
    }
    // Joiner A releases its slot on volume 0: slot A is local again, slot
    // B stays foreign (the withdrawal is per volume).
    let (client_a, joiner_a) = &mut clients[0];
    let words = squeezefs::meta_ship::manager::WireSlotWords::default();
    client_a
        .release_slot(*joiner_a, slot_a, 1, words, Vec::new())
        .await
        .unwrap();
    assert!(squeezefs::dlm_slot::is_local_slot(u64::from(slot_a)));
    assert!(!squeezefs::dlm_slot::is_local_slot(u64::from(slot_b)));
    assert_eq!(squeezefs::dlm_slot::dlm_mode(), "slot-homed");
    host.shutdown();
    // The set's leave withdraws the last contributor: solo.
    shutdown(&routed).await;
    assert_eq!(squeezefs::dlm_slot::dlm_mode(), "solo");
    assert!(squeezefs::dlm_slot::is_local_slot(u64::from(slot_b)));
}

// ---------------------------------------------------------------------------
// Review round 2 — the record-seq space across rings (Issue 2).
// ---------------------------------------------------------------------------

/// **Issue 2 (round 1): a same-key write after a handover from a BUSIER
/// ring to a QUIETER one is the newest value in RAM and at replay.** Record
/// seqs were ring POSITIONS in each ring's own space, and the leaf fold is
/// seq-LWW over every source, so the requester's first write on a key the
/// departing holder had written carried a LOWER seq than the stored one —
/// shadowed in RAM, skipped by the replay gate. The law now: every ring
/// stamps `position + seq_offset`, the release records the departing
/// ring's stamp frontier as the slot's `seq_floor`, and a grant raises the
/// receiving ring's offset above it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_same_key_write_after_a_handover_to_a_quieter_ring_is_the_newest_value() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-000000000000000b");
    let owner = ino_in_slot(SLOT4, 5);
    let digest = {
        let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
        let vol = Arc::clone(&routed.volumes[0]);
        // Region 1 (the busy holder) writes 400 entries of slot 4 — its
        // ring's positions run far past ring 0's, which stays idle.
        for i in 0..400u64 {
            vol.commit_block_refs(owner, &refs(tag, owner, i, 1))
                .await
                .unwrap();
        }
        let head1 = vol.ring_of_region(1).core().head();
        let head0 = vol.journal_ring().core().head();
        assert!(
            head1 > head0 + 10_000,
            "ring 1 {head1} is the busy one; ring 0 {head0} idle"
        );
        assert_eq!(vol.block_ref_count(tag, 395).await.unwrap(), 1);
        // The handover to the manager (region 0).
        vol.manager_offer_slot(1, SLOT4, 0).await.unwrap();
        assert_eq!(
            vol.journal_ring().seq_offset(),
            0,
            "ring 0 stamped its positions so far"
        );
        let AcquireSlotReply::Granted(g) = vol.manager_acquire_slot(0, SLOT4).await.unwrap() else {
            panic!()
        };
        assert_eq!(g.g, 2);
        // The law's mechanism: the release recorded ring 1's stamp frontier
        // as the slot's floor and the grant raised ring 0's offset above it.
        assert!(
            g.words.seq_floor >= head1,
            "the floor is the departing ring's frontier"
        );
        assert!(
            vol.journal_ring().seq_frontier() > g.words.seq_floor,
            "ring 0 stamps above the floor now (offset {})",
            vol.journal_ring().seq_offset()
        );
        assert!(vol.journal_ring().seq_offset() > 0);
        // The requester's SAME-KEY write: release block 395's reference (a
        // Delete of a key region 1 Put LATE in its ring — a seq far past
        // ring 0's head).
        let op = BlockRefOp::released(BlockRef {
            vol_tag: tag,
            block_idx: 395,
            owner_ino: owner,
            block_index: 0,
        });
        vol.commit_block_refs(owner, &[op]).await.unwrap();
        assert_eq!(
            vol.block_ref_count(tag, 395).await.unwrap(),
            0,
            "the acked release is the newest value in RAM (the overlay head)"
        );
        // A checkpoint freezes the overlay into the leaf's bset beside the
        // departing holder's records: the fold is seq-LWW there.
        vol.checkpoint_now().await.unwrap();
        assert_eq!(
            vol.block_ref_count(tag, 395).await.unwrap(),
            0,
            "the acked release is the newest value in RAM after the freeze"
        );
        // A second release, left in ring 0's window for the replay gate.
        let op = BlockRefOp::released(BlockRef {
            vol_tag: tag,
            block_idx: 397,
            owner_ino: owner,
            block_index: 0,
        });
        vol.commit_block_refs(owner, &[op]).await.unwrap();
        assert_eq!(vol.block_ref_count(tag, 397).await.unwrap(), 0);
        vol.sync_device().await.unwrap();
        let d = digest_backend(&vol).await.unwrap();
        // Crash: no leave.
        drop(vol);
        drop(routed);
        d
    };
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    assert_eq!(
        vol.block_ref_count(tag, 395).await.unwrap(),
        0,
        "the acked release survives the remount"
    );
    assert_eq!(vol.block_ref_count(tag, 396).await.unwrap(), 1);
    assert_eq!(
        vol.block_ref_count(tag, 397).await.unwrap(),
        0,
        "the acked in-window release is the newest value at replay"
    );
    assert_eq!(digest_backend(&vol).await.unwrap(), digest);
    shutdown(&routed).await;
}

/// **Issue 3 (round 1): a stale `ReleaseSlot` never rewrites tree 0.**
/// The durable-witness pre-check let an `Unleased`-at-another-`g` release
/// PROCEED — tree 0 was rewritten with the caller's stale words and lower
/// `g` before the table refused — so a wire holder's retried release
/// after a re-lease rolled the slot's record back (the old root served,
/// the cursor floor lost). The verdict now precedes the write: the g = 1
/// release replayed after the g = 2 release is REFUSED, counted on
/// `manager_verb_refusals`, and tree 0 keeps the g = 2 record verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_release_is_refused_before_tree0_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let tag = volume_tag("vol-000000000000000c");
    let slot: ForestSlot = 6; // a rotor slot of region 0
    let owner = ino_in_slot(slot, 4);
    vol.commit_block_refs(owner, &refs(tag, owner, 0, 2))
        .await
        .unwrap();
    // Release at g = 1 (the words as of now), re-grant (g = 2), write
    // more, release at g = 2 (newer root, higher cursor floor).
    vol.release_slot_handover(0, slot).await.unwrap();
    let states = tree0_states(&vol).await;
    let Some((
        _,
        SlotState::Unleased {
            g: 1, root: root1, ..
        },
    )) = states.iter().find(|(s, _)| *s == slot)
    else {
        panic!("{states:?}");
    };
    let root1 = *root1;
    let g2 = vol
        .manager_acquire_slots(0, 0, &[slot], ControlAdmit::Try)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(g2.g, 2);
    vol.commit_block_refs(owner, &refs(tag, owner, 10, 3))
        .await
        .unwrap();
    vol.release_slot_handover(0, slot).await.unwrap();
    let states = tree0_states(&vol).await;
    let Some((_, before @ SlotState::Unleased { g: 2, .. })) =
        states.iter().find(|(s, _)| *s == slot)
    else {
        panic!("{states:?}");
    };
    let before = before.clone();
    let refusals_before = vol.appender_stats().unwrap().manager_verb_refusals;
    // The stale replay: the g = 1 release, with the g = 1 words.
    let stale = squeezefs::slot_lease_core::SlotWords {
        root: (root1.addr, root1.seq),
        cursor: 0,
        extents: 1,
        seq_floor: 0,
    };
    let e = vol
        .manager_release_slot(0, slot, stale, 1, Vec::new())
        .await
        .expect_err("a release at a stale g is refused");
    assert!(e.to_string().contains("refused"), "{e}");
    assert_eq!(
        vol.appender_stats().unwrap().manager_verb_refusals,
        refusals_before + 1,
        "the witness contradicted the caller"
    );
    let states = tree0_states(&vol).await;
    assert_eq!(
        states.iter().find(|(s, _)| *s == slot).map(|(_, st)| st),
        Some(&before),
        "tree 0 is untouched by the stale release"
    );
    // And the true replay of the g = 2 release is `already`, no write.
    let entries_before = vol.journal_ring().written_entries();
    let SlotState::Unleased {
        root,
        cursor,
        slot_tree_extents,
        seq_floor,
        ..
    } = &before
    else {
        unreachable!()
    };
    let same = squeezefs::slot_lease_core::SlotWords {
        root: (root.addr, root.seq),
        cursor: *cursor,
        extents: *slot_tree_extents,
        seq_floor: *seq_floor,
    };
    assert!(vol
        .manager_release_slot(0, slot, same, 2, Vec::new())
        .await
        .unwrap());
    assert_eq!(vol.journal_ring().written_entries(), entries_before);
    shutdown(&routed).await;
}

/// The two-sided grant closure on every path a slot tree changes hands
/// (review round 2, Issue 4): `extent_grant_extents ≡ claimed + returned
/// + unclaimed` holds after a HANDOVER and after a clean LEAVE, the
/// departing region's grant RECORD stops naming the released trees' live
/// images on both paths, and a compaction by the new lessee after the
/// leave leaves fsck C13 empty — the first build moved the images with
/// the handover only, so a leave left them claimed in the old record for
/// C13 to "return" (a double free once the new lessee retired them).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leave_moves_the_released_trees_images_out_of_the_regions_grant() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let tag = volume_tag("vol-000000000000000d");
    let owner = ino_in_slot(SLOT4, 5);
    let closure = |s: &squeezefs::meta_backend::kv::appender::AppenderStats| {
        assert_eq!(
            s.grant_granted,
            s.grant_claimed + s.grant_returned + s.grant_unclaimed,
            "extent_grant_extents ≡ claimed + returned + unclaimed: {s:?}"
        );
    };
    let images = {
        let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
        let vol = Arc::clone(&routed.volumes[0]);
        for i in 0..40u64 {
            vol.commit_block_refs(owner, &refs(tag, owner, i * 8, 4))
                .await
                .unwrap();
        }
        vol.checkpoint_now().await.unwrap();
        // Region 1's grant claims the slot-4 tree's images; the record
        // names them.
        let tree = vol
            .all_trees()
            .into_iter()
            .find(|t| t.forest_slot() == Some(SLOT4))
            .expect("slot 4's tree");
        let heap = vol.superblock().heap.start;
        let node = u64::from(vol.superblock().node_size);
        let images: Vec<u64> = tree
            .reachable_node_addrs()
            .await
            .unwrap()
            .into_iter()
            .map(|a| (a - heap) / node)
            .collect();
        assert!(!images.is_empty());
        let record = vol.extent_grant_record(1).await.unwrap();
        assert!(
            images.iter().all(|e| record.contains(*e)),
            "region 1's record claims its tree's images"
        );
        closure(&vol.appender_stats().unwrap());
        // The clean leave.
        shutdown(&routed).await;
        images
    };
    // The remount WITHOUT the declared region: the manager takes slot 4
    // by first touch and compacts the tree it inherited.
    let routed = open_under(&uris, &Knobs::armed()).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let record = vol.extent_grant_record(1).await.unwrap();
    assert!(
        images.iter().all(|e| !record.contains(*e)),
        "the leave moved the released tree's images out of region 1's record: {:?} ∩ {:?}",
        images,
        record.runs
    );
    closure(&vol.appender_stats().unwrap());
    for i in 40..400u64 {
        vol.commit_block_refs(owner, &refs(tag, owner, i * 8, 4))
            .await
            .unwrap();
    }
    vol.checkpoint_now().await.unwrap();
    assert!(
        vol.c13_orphan_image_extents().await.unwrap().is_empty(),
        "no grant claims an image the new lessee retired"
    );
    // And after a HANDOVER back to a declared region the closure holds on
    // both sides too.
    shutdown(&routed).await;
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let set = vol.appender_stats().unwrap();
    assert_eq!(
        set.regions[1].leases, 1,
        "the wish-list took the unleased slot 4 back"
    );
    closure(&set);
    vol.commit_block_refs(owner, &refs(tag, owner, 4_000, 4))
        .await
        .unwrap();
    vol.manager_offer_slot(1, SLOT4, 0).await.unwrap();
    let AcquireSlotReply::Granted(_) = vol.manager_acquire_slot(0, SLOT4).await.unwrap() else {
        panic!()
    };
    closure(&vol.appender_stats().unwrap());
    let record = vol.extent_grant_record(1).await.unwrap();
    let tree = vol
        .all_trees()
        .into_iter()
        .find(|t| t.forest_slot() == Some(SLOT4))
        .unwrap();
    let heap = vol.superblock().heap.start;
    let node = u64::from(vol.superblock().node_size);
    for a in tree.reachable_node_addrs().await.unwrap() {
        assert!(
            !record.contains((a - heap) / node),
            "the handover moved the images too"
        );
    }
    assert!(vol.c13_orphan_image_extents().await.unwrap().is_empty());
    shutdown(&routed).await;
}

/// **Issue 8 (round 1): a leave-crash history is not a custody conflict.**
/// The mount dies during its clean leave after its leases went `Unleased`
/// in tree 0 and before its pages went `Free`; another appender then
/// leases one of those slots (g + 1). The crashed identity's remount must
/// SUCCEED — the page's attestation at the lower `g` is stale residue
/// (`g` is strictly monotone per slot and tree 0 is the witness), dropped
/// and counted, never `slot_lease_conflicts` — and the leave now writes
/// the design's `Releasing` page step before the tree-0 batch, so the
/// residue is a `Releasing` entry the row-6 rule already drops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leave_crash_history_remounts_without_a_custody_conflict() {
    use squeezefs::meta_backend::kv::backend::TEST_LEAVE_HOLD_AFTER_RELEASES;
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let path = std::path::Path::new(&uris[0]);
    let tag = volume_tag("vol-000000000000000e");
    let owner = ino_in_slot(SLOT4, 5);
    // 1. The declared region leases slot 4 and writes it; the leave dies
    //    between the tree-0 batch and the pages going Free.
    {
        let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
        let vol = Arc::clone(&routed.volumes[0]);
        for i in 0..4u64 {
            vol.commit_block_refs(owner, &refs(tag, owner, i * 8, 2))
                .await
                .unwrap();
        }
        TEST_LEAVE_HOLD_AFTER_RELEASES.store(true, Ordering::Relaxed);
        shutdown(&routed).await;
        TEST_LEAVE_HOLD_AFTER_RELEASES.store(false, Ordering::Relaxed);
        drop(vol);
        let entries = read_directory(path, routed.volumes[0].superblock())
            .await
            .unwrap();
        let p1 = entries[1].page.clone().unwrap();
        assert_eq!(
            p1.state,
            AppenderState::Live,
            "the crash left the page Live"
        );
        let e4 = p1
            .slots
            .iter()
            .find(|e| guest_forest_slot(e.slot) == SLOT4)
            .expect("… still attesting slot 4");
        assert_eq!(
            e4.state,
            SlotEntryState::Releasing,
            "the leave's first step is the page's Releasing attestation (§5.3.4)"
        );
    }
    // 2. Another appender leases slot 4 and still HOLDS it: a manager-only
    //    mount (the seam's region undeclared — its Live page is listed,
    //    not recovered) takes it by first touch (g = 2) and crashes with
    //    its page attesting the slot.
    let sb = {
        let routed = open_under(&uris, &Knobs::armed()).await;
        let vol = Arc::clone(&routed.volumes[0]);
        vol.commit_block_refs(owner, &refs(tag, owner, 100, 1))
            .await
            .unwrap();
        let states = tree0_states(&vol).await;
        assert!(matches!(
            states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st),
            Some(SlotState::Leased {
                appender_id: 0,
                g: 2,
                ..
            })
        ));
        vol.checkpoint_now().await.unwrap();
        vol.sync_device().await.unwrap();
        let sb = vol.superblock().clone();
        drop(vol);
        drop(routed);
        sb
    };
    // Forge the pre-fix residue: page 1's entries flipped back to LIVE at
    // their g = 1 — the shape a leave that skipped the Releasing step (or a
    // crash between it and the page write) leaves behind. The settle must
    // read a Live attestation BELOW tree 0's g as stale, never a conflict.
    {
        let entries = read_directory(path, &sb).await.unwrap();
        let mut p1 = entries[1].page.clone().unwrap();
        for e in &mut p1.slots {
            e.state = SlotEntryState::Live;
        }
        p1.generation += 1;
        let img = p1.encode().unwrap();
        for off in entries[1].dir_offsets {
            write_page(path, off, img.clone()).await.unwrap();
        }
    }
    // 3. The crashed identity remounts with its partition: page 0 (own
    //    residue) attests slot 4 at g = 2, page 1 at g = 1 — the lower g
    //    is stale, dropped and counted; the mount succeeds, no conflict.
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let s = lease_stats(&vol);
    assert_eq!(
        s.conflicts, 0,
        "a leave-crash history is not a C14 conflict"
    );
    assert_eq!(
        s.stale_entries, 1,
        "the lower-g Live attestation is dropped and COUNTED"
    );
    assert_eq!(
        vol.appender_stats().unwrap().regions[1].leases,
        0,
        "slot 4 is region 0's"
    );
    let states = tree0_states(&vol).await;
    assert!(matches!(
        states.iter().find(|(s, _)| *s == SLOT4).map(|(_, st)| st),
        Some(SlotState::Leased {
            appender_id: 0,
            g: 2,
            ..
        })
    ));
    for i in 0..4u64 {
        assert_eq!(vol.block_ref_count(tag, i * 8).await.unwrap(), 1);
    }
    assert_eq!(vol.block_ref_count(tag, 100).await.unwrap(), 1);
    shutdown(&routed).await;
}

// ---------------------------------------------------------------------------
// Review round 3 — the handover's flush over a tree with pending structure
// (found by Issue 21's large-tree fixture).
// ---------------------------------------------------------------------------

/// **The departing holder's flush is the release's own work.** A slot
/// tree with pending SMOs at the handover — leaves past their split
/// threshold, the shape every tree of operating size has — must still
/// release: flush-then-transfer runs the holder's compactions and splits
/// under `Releasing`, and the third gate state exists to refuse USER
/// commits the door already drained, never the flush pass's own moves.
/// Before the fix the SMO's leftover move into its successor was refused
/// at `apply_locked` ("the slot is mid-handover") and the release failed
/// — every tree larger than the suite's 1–10-leaf fixtures.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handover_of_a_tree_with_pending_structure_runs_the_holders_own_smos() {
    let dir = tempfile::tempdir().unwrap();
    let _g = SEAM.lock().await;
    let uris = vec![format_stamped_member(dir.path(), "meta0").await];
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    // ≈ 200 leaves of 15 KiB xattr values, the last of them dirty and
    // past their split threshold when the release's flush runs.
    let value = vec![0x3Cu8; 15 * 1024];
    let ledger = Arc::clone(&vol.slot_leases().unwrap().extents);
    let mut k = 0u64;
    while ledger.get(SLOT4) < 200 {
        for _ in 0..64 {
            vol.setxattr_internal(ino_in_slot(SLOT4, 1 + k / 8), &format!("user.t{k}"), &value)
                .await
                .unwrap();
            k += 1;
        }
        assert!(k < 50_000, "the tree never reached 200 extents");
    }
    let refusals0 =
        squeezefs::meta_backend::kv::META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed);
    vol.release_slot_handover(1, SLOT4)
        .await
        .expect("the release lands over a tree with pending structure");
    assert_eq!(
        squeezefs::meta_backend::kv::META_KV_LEAF_LEASE_REFUSALS.load(Ordering::Relaxed),
        refusals0,
        "the holder's own flush is never refused by its own gate"
    );
    assert!(!vol.slot_leases().unwrap().gate.is_leased(SLOT4));
    // Every record survives the release and a remount.
    let probe = ino_in_slot(SLOT4, 1);
    let v = vol.getxattr(probe, "user.t0").await.unwrap();
    assert_eq!(v.as_deref(), Some(value.as_slice()));
    drop(vol);
    drop(routed);
    let routed = open_under(&uris, &Knobs::armed().partition(PARTITION_ALT)).await;
    let vol = Arc::clone(&routed.volumes[0]);
    let v = vol.getxattr(probe, "user.t0").await.unwrap();
    assert_eq!(v.as_deref(), Some(value.as_slice()));
    shutdown(&routed).await;
}
