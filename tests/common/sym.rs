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

// ---------------------------------------------------------------------------
// The DATA rig: the routed set bound to a file-backed data volume through
// a `DataRouter`, with PR 7's arm run (`arm_shared_refs`).
// ---------------------------------------------------------------------------

use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::nvme_dev::NvmeBlockDev;
use squeezefs::routing::{BlockMapOp, DataRouter, LayoutFlip};
use tempfile::{tempdir, NamedTempFile, TempDir};

pub const DATA_LEN: u64 = 2 * 1024 * 1024 * 1024;
pub const DATA_VOL: &str = "vol-00000000000000b3";

pub struct DataRig {
    pub router: DataRouter,
    pub alloc: Arc<BlockAllocator>,
    pub routed: Arc<RoutedMetaBackend>,
    _staging: TempDir,
}

pub fn data_file() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    std::fs::File::create(f.path())
        .unwrap()
        .set_len(DATA_LEN)
        .unwrap();
    f
}

pub async fn mount_data(uris: &[String], data: &std::path::Path, knobs: &Knobs) -> DataRig {
    let routed = open_under(uris, knobs).await;
    let dlm = DlmClient::new().unwrap();
    let nvme = Arc::new(NvmeBlockDev::new(data.to_str().unwrap()));
    let alloc = Arc::new(BlockAllocator::new(DATA_VOL).await.unwrap());
    alloc.set_capacity_bytes(DATA_LEN);
    let staging = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![staging.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some("32MB"),
        alloc.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm, cache, alloc.clone(), nvme);
    router.set_meta_backend(routed.clone());
    router.arm_shared_refs().await.expect("arm shared refs");
    DataRig {
        router,
        alloc,
        routed,
        _staging: staging,
    }
}

impl DataRig {
    pub fn vol(&self) -> &Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend> {
        &self.routed.volumes[0]
    }

    pub fn tag(&self) -> u64 {
        squeezefs::meta_backend::kv::block_refs::volume_tag(DATA_VOL)
    }

    pub async fn mk_file(&self, name: &str) -> u64 {
        use squeezefs::meta_backend::Metadata;
        self.routed
            .create(1, name, libc::S_IFREG | 0o644, 1000, 1000)
            .await
            .expect("create")
            .ino
    }

    /// A file whose forest slot differs from `not`'s (the rotor spreads
    /// children of `/` over 64 slots; two creates land apart, but the
    /// contract states it rather than presumes it).
    pub async fn mk_file_apart(&self, prefix: &str, not: ForestSlot) -> u64 {
        for i in 0..70 {
            let ino = self.mk_file(&format!("{prefix}{i}")).await;
            if slot_of_global(&self.routed, ino) != not {
                return ino;
            }
        }
        panic!("the rotor never left slot {not}");
    }

    pub fn token(&self, ino: u64) -> u64 {
        self.router.dlm.get_fencing_token_ino(ino)
    }

    /// Allocate a real block and bind it at `block_index` of `ino`
    /// through the shared merge primitive (the one place a striped map
    /// changes — where the accounting is computed); the displaced keys
    /// the primitive hands back are freed after the guard drops (the
    /// write path's own discipline).
    pub async fn publish_block(&self, ino: u64, block_index: u32) -> u64 {
        let offset = self.alloc.allocate_block().await.expect("allocate");
        self.alloc.publish_block(offset);
        let key = offset.to_string();
        let displaced = self
            .router
            .merge_block_mappings(
                ino,
                BlockMapOp::Merge(&[(block_index, key)]),
                (block_index as u64 + 1) * 4 * 1024 * 1024,
                LayoutFlip::ToStripedKeepStagedIdentity,
                self.token(ino),
            )
            .await
            .expect("merge published block");
        for k in displaced {
            self.router
                .backend_router
                .free_block(&k)
                .await
                .expect("free displaced");
        }
        offset
    }

    pub async fn clone(&self, src: u64, dst: u64) -> squeezefs::error::Result<()> {
        self.router
            .clone_file(
                &squeezefs::keys::inode_path(src),
                &squeezefs::keys::inode_path(dst),
                Some(self.token(src)),
                Some(self.token(dst)),
            )
            .await
    }

    pub async fn drift(&self) -> Vec<(String, u64, u32, u32)> {
        self.router
            .backend_router
            .verify_durable_block_refs(&self.routed)
            .await
            .expect("C8 oracle")
    }

    pub async fn shutdown(self) {
        shutdown(&self.routed).await;
    }
}
