//! The armed-forest harness (the `sym_slot_transfer_tests` shape: 64 KiB
//! nodes, a 1 MiB fixed ring, one stamped member, the plane armed through
//! the registered knob and the non-PR lab opt-in). Every knob is
//! process-global — a suite serializes its tests on [`SEAM`].

#![allow(dead_code)]

use squeezefs::meta_backend::kv::appender::TEST_APPENDER_SLOTS_ENV;
use squeezefs::meta_backend::kv::builder::{format_v3_stamped, FormatV3Options};
use squeezefs::meta_backend::kv::record::{forest_slot_of_ino, ForestSlot};
use squeezefs::meta_backend::kv::slot_lease::{
    SYMMETRIC_META_ENV, SYM_AFFINITY_MAX_MB_ENV, SYM_MINT_SLOTS_ENV, SYM_T_IDLE_MS_ENV,
};
use squeezefs::meta_backend::{open_routed_meta_set, plan_meta_slot_set, RoutedMetaBackend};
use std::sync::Arc;

pub const VOL_LEN: u64 = 64 * 1024 * 1024;
pub const NODE_SIZE: usize = 64 * 1024;
pub const RING_LEN: u64 = 1024 * 1024;

/// The seams and knobs are process-global; every test serializes on it.
pub static SEAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn set_opts() -> FormatV3Options {
    FormatV3Options {
        node_size: NODE_SIZE,
        journal_len_override: Some(RING_LEN),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    }
}

/// One member formatted under the bit-17 seam.
pub async fn format_stamped_member(dir: &std::path::Path, name: &str) -> String {
    let p = dir.join(name);
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("derived plan");
    std::env::set_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC", "1");
    let r = format_v3_stamped(&p, VOL_LEN, &set_opts(), plan.stamps[0].clone()).await;
    std::env::remove_var("SQUEEZEFS_TEST_STAMP_SYMMETRIC");
    r.expect("format stamped member");
    p.display().to_string()
}

/// One member formatted FLAT (the seam cleared).
pub async fn format_flat_member(dir: &std::path::Path, name: &str) -> String {
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
pub struct Knobs {
    pub armed: bool,
    pub partition: Option<&'static str>,
    pub mint_slots: Option<&'static str>,
    pub affinity_mb: Option<&'static str>,
    pub t_idle_ms: Option<&'static str>,
}

impl Knobs {
    pub fn armed() -> Self {
        Self {
            armed: true,
            partition: None,
            mint_slots: None,
            affinity_mb: None,
            t_idle_ms: None,
        }
    }
    pub fn unarmed() -> Self {
        Self {
            armed: false,
            ..Self::armed()
        }
    }
    pub fn partition(mut self, p: &'static str) -> Self {
        self.partition = Some(p);
        self
    }
    pub fn mint_slots(mut self, m: &'static str) -> Self {
        self.mint_slots = Some(m);
        self
    }
    pub fn affinity_mb(mut self, mb: &'static str) -> Self {
        self.affinity_mb = Some(mb);
        self
    }
    pub fn t_idle_ms(mut self, ms: &'static str) -> Self {
        self.t_idle_ms = Some(ms);
        self
    }
    pub fn apply(&self) {
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
    pub fn clear() {
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
pub async fn open_under(uris: &[String], knobs: &Knobs) -> Arc<RoutedMetaBackend> {
    knobs.apply();
    let r = open_routed_meta_set(uris).await;
    Knobs::clear();
    r.expect("open routed set")
}

/// An ino inside forest slot `slot`'s guest keyspace (local key ino).
pub fn ino_in_slot(slot: ForestSlot, local: u64) -> u64 {
    squeezefs::meta_backend::guest_local_ino((slot - 1) as u16, local)
}

/// The forest slot a GLOBAL ino's records live in on the routed set.
pub fn slot_of_global(routed: &RoutedMetaBackend, ino: u64) -> ForestSlot {
    let (_v, local) = routed.route_ino(ino);
    forest_slot_of_ino(local)
}

pub async fn shutdown(routed: &RoutedMetaBackend) {
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}
