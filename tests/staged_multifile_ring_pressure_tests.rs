//! The aged zeros-LOSS repro, deterministic at cargo level: N staged files
//! COMPETING for one small ring — the aged ingredients the single-file
//! mini-fsx lacked. Same-key replaces refuse under fragmentation (spill
//! leg), every stage crosses the high-water mark (constant promotion
//! churn), and identities cycle ring→durable→ring. Each worker runs an
//! fsx-shaped op mix (write / extend / truncate up+down / punch / zero /
//! whole-file rewrite) against its own byte model and verifies FULL-image
//! byte-exactness after every op. Any acked byte reading zeros — the aged
//! fsx GOOD→0x0000 class — fails with the op trace.
//!
//! Seeded and bounded: three seeds × bounded rounds, minutes not soaks.

use fuse3::raw::prelude::Filesystem;
use fuse3::raw::Request;
use squeezefs::block_allocator::BlockAllocator;
use squeezefs::cache::TieredCache;
use squeezefs::dlm::DlmClient;
use squeezefs::fuse_client::SqueezefsFilesystem;
use squeezefs::meta_backend::kv::backend::KvMetaBackend;
use squeezefs::meta_backend::kv::builder::{BuilderConfig, ImageBuilder};
use squeezefs::meta_backend::kv::node::DEFAULT_NODE_SIZE;
use squeezefs::meta_backend::RoutedMetaBackend;
use squeezefs::routing::DataRouter;
use std::ffi::OsStr;
use std::sync::Arc;
use tempfile::{tempdir, NamedTempFile, TempDir};

struct H {
    fs: SqueezefsFilesystem,
    req: Request,
    _b: NamedTempFile,
    _m: NamedTempFile,
    _s: TempDir,
}

async fn make(uuid: [u8; 16], alloc_ns: &str, budget: &str) -> H {
    std::env::remove_var("SQUEEZEFS_DEFAULT_BLOCK_SIZE");
    let dlm = DlmClient::new().unwrap();
    let b = NamedTempFile::new().unwrap();
    std::fs::File::create(b.path())
        .unwrap()
        .set_len(512 * 1024 * 1024)
        .unwrap();
    let nvme = Arc::new(squeezefs::nvme_dev::NvmeBlockDev::new(
        b.path().to_str().unwrap(),
    ));
    let ba = Arc::new(BlockAllocator::new(alloc_ns).await.unwrap());
    let s = tempdir().unwrap();
    let cache = TieredCache::new(
        vec![s.path().to_path_buf()],
        Some("64MB"),
        Some("64MB"),
        Some("16MB"),
        Some(budget),
        ba.clone(),
        nvme.clone(),
        None,
    )
    .await
    .unwrap();
    let router = DataRouter::new(dlm.clone(), cache, ba, nvme);
    let mut fs = SqueezefsFilesystem::new(router, dlm.clone(), 1000, 1000);
    let m = NamedTempFile::new().unwrap();
    m.as_file().set_len(128 * 1024 * 1024).unwrap();
    let routed: Arc<RoutedMetaBackend> = {
        ImageBuilder::new(BuilderConfig {
            node_size: DEFAULT_NODE_SIZE,
            journal_len_override: None,
            hash_seed: 0xC0FF_EE00_1234_5678,
            uuid,
        })
        .unwrap()
        .build(m.path(), 128 * 1024 * 1024)
        .await
        .unwrap();
        let be = KvMetaBackend::open(m.path()).await.unwrap();
        Arc::new(RoutedMetaBackend::new(vec![be]))
    };
    fs.router.set_meta_backend(routed.clone());
    fs.meta_backend = Some(routed);
    let req = Request {
        unique: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        pid: 1,
        ..Default::default()
    };
    H {
        fs,
        req,
        _b: b,
        _m: m,
        _s: s,
    }
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
}

async fn run_worker(h: Arc<H>, ino: u64, seed: u64, rounds: usize, tag: String) {
    const MAXLEN: u64 = 900 * 1024;
    let mut rng = Lcg(seed);
    let mut model: Vec<u8> = Vec::new();
    let mut opn = 0u64;
    let mut history: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let note = |s: String, history: &mut std::collections::VecDeque<String>| {
        history.push_back(s);
        if history.len() > 12 {
            history.pop_front();
        }
    };

    for _ in 0..rounds {
        opn += 1;
        let r = rng.next() % 100;
        match r {
            // Extend/overwrite write.
            0..=39 => {
                let off = rng.next() % MAXLEN.min(model.len() as u64 + 128 * 1024);
                let len = (4096 + rng.next() % (96 * 1024)).min(MAXLEN - off);
                let val = (opn % 251) as u8 | 1;
                let buf = vec![val; len as usize];
                let w =
                    h.fs.write(
                        h.req,
                        ino,
                        0,
                        off,
                        bytes::Bytes::copy_from_slice(&buf),
                        0,
                        0,
                    )
                    .await
                    .unwrap();
                assert_eq!(w.written as u64, len, "{tag} op{opn}: short write");
                if (off + len) as usize > model.len() {
                    model.resize((off + len) as usize, 0);
                }
                model[off as usize..(off + len) as usize].fill(val);
                note(
                    format!("op{opn} write [{off:#x},{:#x})", off + len),
                    &mut history,
                );
            }
            // Truncate (up or down).
            40..=54 => {
                let ns = rng.next() % MAXLEN;
                h.fs.setattr(
                    h.req,
                    ino,
                    None,
                    fuse3::SetAttr {
                        size: Some(ns),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                model.resize(ns as usize, 0);
                note(format!("op{opn} truncate -> {ns:#x}"), &mut history);
            }
            // Punch hole (keep size).
            55..=69 => {
                if model.is_empty() {
                    continue;
                }
                let off = rng.next() % model.len() as u64;
                let len = (4096 + rng.next() % (64 * 1024)).min(model.len() as u64 - off);
                if len == 0 {
                    continue;
                }
                h.fs.fallocate(
                    h.req,
                    ino,
                    0,
                    off,
                    len,
                    (libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE) as u32,
                )
                .await
                .unwrap();
                model[off as usize..(off + len) as usize].fill(0);
                note(
                    format!("op{opn} punch [{off:#x},{:#x})", off + len),
                    &mut history,
                );
            }
            // Extending fallocate / zero_range-grow — the extend_file_size
            // path, racing the merge worker's promotion commits (the aged
            // EIO-wedge + payload-stranding leg: an unlocked stale-snapshot
            // whole-meta save erased a just-published block_map[0] right
            // after the ring entry was released).
            70..=74 => {
                let off = model.len() as u64;
                let len = 4096 + rng.next() % (64 * 1024);
                if off + len > MAXLEN {
                    continue;
                }
                h.fs.fallocate(h.req, ino, 0, off, len, 0).await.unwrap();
                model.resize((off + len) as usize, 0);
                note(
                    format!("op{opn} falloc-extend -> {:#x}", off + len),
                    &mut history,
                );
            }
            // Zero range (keep size).
            75..=79 => {
                if model.is_empty() {
                    continue;
                }
                let off = rng.next() % model.len() as u64;
                let len = (4096 + rng.next() % (64 * 1024)).min(model.len() as u64 - off);
                if len == 0 {
                    continue;
                }
                h.fs.fallocate(
                    h.req,
                    ino,
                    0,
                    off,
                    len,
                    (libc::FALLOC_FL_ZERO_RANGE | libc::FALLOC_FL_KEEP_SIZE) as u32,
                )
                .await
                .unwrap();
                model[off as usize..(off + len) as usize].fill(0);
                note(
                    format!("op{opn} zero [{off:#x},{:#x})", off + len),
                    &mut history,
                );
            }
            // Whole-file rewrite (the ring-replace/spill pressure shape).
            _ => {
                let len = 128 * 1024 + rng.next() % (MAXLEN - 128 * 1024);
                let val = (opn % 251) as u8 | 1;
                let buf = vec![val; len as usize];
                h.fs.setattr(
                    h.req,
                    ino,
                    None,
                    fuse3::SetAttr {
                        size: Some(0),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                let w =
                    h.fs.write(h.req, ino, 0, 0, bytes::Bytes::copy_from_slice(&buf), 0, 0)
                        .await
                        .unwrap();
                assert_eq!(w.written as u64, len, "{tag} op{opn}: short rewrite");
                model.clear();
                model.extend_from_slice(&buf);
                note(format!("op{opn} rewrite len={len:#x}"), &mut history);
            }
        }

        // Full-image verification after EVERY op.
        let got = if model.is_empty() {
            Vec::new()
        } else {
            h.fs.read(h.req, ino, 0, 0, model.len() as u32, 0)
                .await
                .unwrap()
                .data
                .to_vec()
        };
        assert_eq!(
            got.len(),
            model.len(),
            "{tag} op{opn}: short read (history: {history:?})"
        );
        if let Some(i) = (0..got.len()).find(|&i| got[i] != model[i]) {
            let zeros = got
                .iter()
                .zip(model.iter())
                .filter(|(g, m)| **g == 0 && **m != 0)
                .count();
            panic!(
                "{tag} op{opn}: first mismatch at {i:#x} of {:#x}: got {:#04x} want {:#04x} \
                 ({zeros} acked bytes read ZERO — the aged loss class); history: {history:?}",
                model.len(),
                got[i],
                model[i]
            );
        }
    }
}

async fn multifile_pressure(seed: u64, files: usize, rounds: usize, uuid: [u8; 16], ns: &str) {
    let h = Arc::new(make(uuid, ns, "2MB").await);
    let mut inos = Vec::new();
    for i in 0..files {
        inos.push(
            h.fs.create(
                h.req,
                1,
                OsStr::new(&format!("mfp_{i}")),
                libc::S_IFREG | 0o644,
                0,
            )
            .await
            .unwrap()
            .attr
            .ino,
        );
    }
    let mut tasks = Vec::new();
    for (i, &ino) in inos.iter().enumerate() {
        let h = h.clone();
        let tag = format!("seed{seed}/file{i}");
        tasks.push(tokio::spawn(run_worker(
            h,
            ino,
            seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
            rounds,
            tag,
        )));
    }
    for t in tasks {
        t.await.expect("worker panicked");
    }
}

/// Directed hammer for the extend-vs-promotion race: tight
/// write → fallocate-extend → verify loops while the tiny ring keeps a
/// promotion of the same file perpetually in flight. The unlocked
/// stale-snapshot save in the extend path lands in the µs window right
/// after a promotion commit publishes block_map[0] and releases the ring
/// entry — erasing the mapping and stranding the payload (aged signature:
/// 65-re-resolve EIO reads / staged_payload_lost_reads, or durable zeros
/// once an RMW codifies the empty seed).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn extend_vs_promotion_never_strands_payload() {
    const FILES: usize = 8;
    const ROUNDS: usize = 150;
    let h = Arc::new(make(*b"szl-mfp-extend01", "mfp_ns_x", "1MB").await);
    let mut tasks = Vec::new();
    for i in 0..FILES {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let ino =
                h.fs.create(
                    h.req,
                    1,
                    OsStr::new(&format!("ext_{i}")),
                    libc::S_IFREG | 0o644,
                    0,
                )
                .await
                .unwrap()
                .attr
                .ino;
            let mut rng = Lcg(0x00E0_0000 + i as u64);
            for round in 0..ROUNDS {
                // Body write large enough to cross the ring high-water mark
                // => promotion of THIS file is enqueued while we extend.
                let body = 192 * 1024 + (rng.next() % (64 * 1024)) as usize;
                let val = (round % 249) as u8 | 1;
                let w =
                    h.fs.write(h.req, ino, 0, 0, bytes::Bytes::from(vec![val; body]), 0, 0)
                        .await
                        .unwrap();
                assert_eq!(w.written as usize, body);
                // Truncate down to re-arm (next round's write is a fresh image).
                let ext = body as u64 + 4096 + rng.next() % (32 * 1024);
                h.fs.fallocate(h.req, ino, 0, 0, ext, 0).await.unwrap();

                let got =
                    h.fs.read(h.req, ino, 0, 0, ext as u32, 0)
                        .await
                        .unwrap_or_else(|e| {
                            panic!(
                                "file{i} round {round}: read failed {e:?} — the stranded-payload \
                             wedge (ring entry gone AND mapping erased)"
                            )
                        })
                        .data
                        .to_vec();
                assert_eq!(got.len(), ext as usize, "file{i} round {round}: short read");
                if let Some(p) = got[..body].iter().position(|&b| b != val) {
                    panic!(
                        "file{i} round {round}: acked byte at {p} reads {:#04x} (want {val:#04x}) \
                         — extend-vs-promotion stranding/zeros",
                        got[p]
                    );
                }
                assert!(
                    got[body..].iter().all(|&b| b == 0),
                    "file{i} round {round}: extend tail must read zeros"
                );
                h.fs.setattr(
                    h.req,
                    ino,
                    None,
                    fuse3::SetAttr {
                        size: Some(0),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
        }));
    }
    for t in tasks {
        t.await.expect("worker panicked");
    }
}

/// Directed hammer for the superseded-mapping DOUBLE-FREE: ring-miss RMW
/// patches against promoted files. The RMW's stage-commit released the
/// durable mapping from its STALE pre-commit snapshot while a racing
/// promotion commit had already displaced-freed the same mapping — the
/// offset was freed twice, reallocated to ANOTHER file's promotion, then
/// punched/overwritten under the new owner: reads and RMW seeds through
/// the clobbered mapping returned another tenant's bytes or zeros (the
/// aged tape: one device offset cycling as map0 across consecutive
/// promotions and into the truncate clip's own allocation).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn promote_patch_cycles_never_clobber_neighbor_mappings() {
    const FILES: usize = 8;
    const ROUNDS: usize = 60;
    let h = Arc::new(make(*b"szl-mfp-dblfree1", "mfp_ns_d", "1MB").await);
    let mut tasks = Vec::new();
    for i in 0..FILES {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let ino =
                h.fs.create(
                    h.req,
                    1,
                    OsStr::new(&format!("dbl_{i}")),
                    libc::S_IFREG | 0o644,
                    0,
                )
                .await
                .unwrap()
                .attr
                .ino;
            let path = squeezefs::keys::inode_path(ino);
            let mut rng = Lcg(0x00DB_1F00 + i as u64);
            let mut model: Vec<u8> = Vec::new();
            for round in 0..ROUNDS {
                // Big write: crosses the ring high-water mark => promotion
                // of this file is enqueued.
                let body = 192 * 1024 + (rng.next() % (64 * 1024)) as usize;
                let val = (round % 249) as u8 | 1;
                h.fs.write(h.req, ino, 0, 0, bytes::Bytes::from(vec![val; body]), 0, 0)
                    .await
                    .unwrap();
                model.clear();
                model.resize(body, val);

                // Wait (bounded) until the promotion PUBLISHES the mapping —
                // the next patch then runs the ring-miss RMW + stale-snapshot
                // release against a fresh re-promotion cycle.
                let _ = tokio::time::timeout(std::time::Duration::from_millis(400), async {
                    loop {
                        let promoted = h
                            .fs
                            .router
                            .metadata_cache
                            .get(&squeezefs::routing::parse_inode_from_path(&path))
                            .and_then(|m| m.block_map.as_ref().and_then(|bm| bm.get(&0).cloned()))
                            .is_some();
                        if promoted {
                            return;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await;

                // Sub-block patches: each is a whole-image RMW re-stage that
                // supersedes (and frees) the promoted mapping, then crosses
                // high-water again and re-promotes.
                for _ in 0..3 {
                    let off = rng.next() % (body as u64 - 4096);
                    h.fs.write(
                        h.req,
                        ino,
                        0,
                        off,
                        bytes::Bytes::from(vec![0xEE; 4096]),
                        0,
                        0,
                    )
                    .await
                    .unwrap();
                    model[off as usize..off as usize + 4096].fill(0xEE);
                }

                let got =
                    h.fs.read(h.req, ino, 0, 0, body as u32, 0)
                        .await
                        .unwrap_or_else(|e| panic!("file{i} round {round}: read failed {e:?}"))
                        .data
                        .to_vec();
                assert_eq!(got.len(), body, "file{i} round {round}: short read");
                if let Some(p) = (0..body).find(|&p| got[p] != model[p]) {
                    let zeros = got.iter().filter(|&&b| b == 0).count();
                    panic!(
                        "file{i} round {round}: byte {p} reads {:#04x} want {:#04x} \
                         ({zeros}/{body} zero) — a neighbor's promotion/free clobbered \
                         this file's durable mapping (the double-free engine)",
                        got[p], model[p]
                    );
                }
            }
        }));
    }
    for t in tasks {
        t.await.expect("worker panicked");
    }
}

/// Directed hammer for the promote-consume-vs-RMW-commit sole-copy free
/// (leg 5): every patch's re-stage crosses the high-water mark (promotion
/// of the SAME image is enqueued while the RMW walks from `stage_write` to
/// its commit lock), and a concurrent fsync loop contends the per-inode
/// meta lock so the promotion can win the lock race. When it does, the old
/// RMW commit published "ring is authoritative, map=None" AFTER the
/// promotion had consumed-and-removed the ring entry, then freed the
/// promotion's just-published mapping — the file's sole surviving copy
/// (aged tape: bare router_free of the live mapping, offset reallocated to
/// two other inodes, the next durable clip reading 100% zeros). Every
/// acked byte must read back exactly, forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn rmw_commit_vs_promotion_never_frees_sole_copy() {
    const FILES: usize = 8;
    const ROUNDS: usize = 350;
    let h = Arc::new(make(*b"szl-mfp-leg5-001", "mfp_ns_l5", "1MB").await);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tasks = Vec::new();
    for i in 0..FILES {
        let h = h.clone();
        let stop = stop.clone();
        tasks.push(tokio::spawn(async move {
            let ino = h
                .fs
                .create(
                    h.req,
                    1,
                    OsStr::new(&format!("l5_{i}")),
                    libc::S_IFREG | 0o644,
                    0,
                )
                .await
                .unwrap()
                .attr
                .ino;
            // fsync contender: takes INODE_META_LOCKS (persist_dirty_layout)
            // in a tight loop, delaying the RMW commit's lock acquisition.
            let hf = h.clone();
            let stopf = stop.clone();
            let fsyncer = tokio::spawn(async move {
                while !stopf.load(std::sync::atomic::Ordering::Acquire) {
                    let _ = hf.fs.fsync(hf.req, ino, 0, false).await;
                    tokio::task::yield_now().await;
                }
            });

            let mut rng = Lcg(0x1E65_0000 + i as u64);
            let mut model: Vec<u8> = Vec::new();
            for round in 0..ROUNDS {
                let body = 160 * 1024 + (rng.next() % (96 * 1024)) as usize;
                let val = (round % 249) as u8 | 1;
                h.fs.write(h.req, ino, 0, 0, bytes::Bytes::from(vec![val; body]), 0, 0)
                    .await
                    .unwrap();
                model.clear();
                model.resize(body, val);
                // Sub-block patches: each re-stage crosses high-water and
                // races its own enqueued promotion to the commit lock.
                for p in 0..4u8 {
                    let off = rng.next() % (body as u64 - 4096);
                    let pv = 0xE0 | p;
                    h.fs.write(h.req, ino, 0, off, bytes::Bytes::from(vec![pv; 4096]), 0, 0)
                        .await
                        .unwrap();
                    model[off as usize..off as usize + 4096].fill(pv);
                }
                let got = h
                    .fs
                    .read(h.req, ino, 0, 0, body as u32, 0)
                    .await
                    .unwrap_or_else(|e| panic!("file{i} round {round}: read failed {e:?}"))
                    .data
                    .to_vec();
                assert_eq!(got.len(), body, "file{i} round {round}: short read");
                if let Some(p) = (0..body).find(|&p| got[p] != model[p]) {
                    let zeros = got.iter().filter(|&&b| b == 0).count();
                    panic!(
                        "file{i} round {round}: byte {p} reads {:#04x} want {:#04x}                          ({zeros}/{body} zero) — the RMW commit freed the promotion's                          sole-copy mapping (leg 5)",
                        got[p], model[p]
                    );
                }
            }
            stop.store(true, std::sync::atomic::Ordering::Release);
            fsyncer.await.unwrap();
        }));
    }
    for t in tasks {
        t.await.expect("worker panicked");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn multifile_ring_pressure_seed_a() {
    multifile_pressure(0x000A_5EED_0001, 6, 120, *b"szl-mfp-seed-a01", "mfp_ns_a").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn multifile_ring_pressure_seed_b() {
    multifile_pressure(0x000B_5EED_0002, 6, 120, *b"szl-mfp-seed-b01", "mfp_ns_b").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn multifile_ring_pressure_seed_c() {
    multifile_pressure(0x000C_5EED_0003, 6, 120, *b"szl-mfp-seed-c01", "mfp_ns_c").await;
}
