//! PR 13c — the gate-1 flat-path attribution instrument
//! (`.benchmarks/2026-09-19-sym-acceptance.md` §3.9.1c): the ROUTED
//! metadata path on a FLAT (bit-17-absent, unarmed) file-backed volume,
//! no FUSE — `create` / `mkdir` / `rename` / `unlink` timed per op, run on
//! the same laptop against the pre-program tip `3228fcb8` and the current
//! tree (A-B-B-A, SCOPING — the venue law: no laptop number is a verdict;
//! the row names SITES). Ignored by default: run by name.
//!
//! `cargo test --release --test meta_flat_path_microbench -- --ignored
//! --nocapture`.

use squeezefs::meta_backend::kv::builder::{format_v3_stamped, FormatV3Options};
use squeezefs::meta_backend::kv::META_KV_TIMES_ECHO_ABSORBED;
use squeezefs::meta_backend::{open_routed_meta_set, plan_meta_slot_set, Metadata};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

const VOL_LEN: u64 = 512 * 1024 * 1024;
const NODE_SIZE: usize = 256 * 1024;
const ROOT: u64 = 1;

fn ops() -> usize {
    std::env::var("META_MICROBENCH_OPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000)
}

fn threads() -> usize {
    std::env::var("META_MICROBENCH_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
}

async fn format_flat(dir: &std::path::Path) -> String {
    let p = dir.join("meta0");
    std::fs::File::create(&p).unwrap().set_len(VOL_LEN).unwrap();
    let plan = plan_meta_slot_set(1).expect("plan");
    let opts = FormatV3Options {
        node_size: NODE_SIZE,
        journal_len_override: Some(32 * 1024 * 1024),
        force: true,
        full_wipe: false,
        format_config_xattr: None,
    };
    format_v3_stamped(&p, VOL_LEN, &opts, plan.stamps[0].clone())
        .await
        .expect("format");
    p.display().to_string()
}

async fn phase<F, Fut>(name: &str, n: usize, t: usize, f: F) -> f64
where
    F: Fn(usize) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let per = n / t;
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for th in 0..t {
        let f = f.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..per {
                f(th * per + i).await;
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let wall = t0.elapsed();
    let us = wall.as_secs_f64() * 1e6 / (per * t) as f64;
    println!(
        "MICROBENCH {name}: {} ops × {t} threads in {:.3} s = {:.2} µs/op ({:.0} ops/s)",
        per * t,
        wall.as_secs_f64(),
        us,
        (per * t) as f64 / wall.as_secs_f64()
    );
    us
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "an instrument, not a gate — run by name"]
async fn flat_path_microbench() {
    let dir = tempfile::tempdir().unwrap();
    let uri = format_flat(dir.path()).await;
    let routed = open_routed_meta_set(&[uri]).await.expect("open");
    let n = ops();
    let t = threads();
    // Per-thread directories so the 4a guards collide only by stripe.
    let mut dirs = Vec::new();
    for th in 0..t {
        let d = routed
            .create(ROOT, &format!("d{th}"), libc::S_IFDIR | 0o755, 0, 0)
            .await
            .unwrap()
            .ino;
        dirs.push(d);
    }
    let dirs: Arc<Vec<u64>> = Arc::new(dirs);
    let per = n / t;
    // Warm-up: one small round of each shape.
    for th in 0..t {
        for i in 0..8 {
            routed
                .create(dirs[th], &format!("w{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .unwrap();
        }
    }
    let r = Arc::clone(&routed);
    let d = Arc::clone(&dirs);
    let create_us = phase("create", n, t, move |i| {
        let r = Arc::clone(&r);
        let d = Arc::clone(&d);
        async move {
            let th = i / per;
            r.create(d[th], &format!("f{i}"), libc::S_IFREG | 0o644, 0, 0)
                .await
                .unwrap();
        }
    })
    .await;
    let r = Arc::clone(&routed);
    let d = Arc::clone(&dirs);
    let mkdir_us = phase("mkdir", n, t, move |i| {
        let r = Arc::clone(&r);
        let d = Arc::clone(&d);
        async move {
            let th = i / per;
            r.create(d[th], &format!("m{i}"), libc::S_IFDIR | 0o755, 0, 0)
                .await
                .unwrap();
        }
    })
    .await;
    let r = Arc::clone(&routed);
    let d = Arc::clone(&dirs);
    let lookup_us = phase("lookup(+)", n, t, move |i| {
        let r = Arc::clone(&r);
        let d = Arc::clone(&d);
        async move {
            let th = i / per;
            r.lookup(d[th], &format!("f{i}")).await.unwrap();
        }
    })
    .await;
    let r = Arc::clone(&routed);
    let d = Arc::clone(&dirs);
    let neg_us = phase("lookup(-)", n, t, move |i| {
        let r = Arc::clone(&r);
        let d = Arc::clone(&d);
        async move {
            let th = i / per;
            let _ = r.lookup(d[th], &format!("nx{i}")).await;
        }
    })
    .await;
    let r = Arc::clone(&routed);
    let d = Arc::clone(&dirs);
    let getattr_us = phase("getattr", n, t, move |i| {
        let r = Arc::clone(&r);
        let d = Arc::clone(&d);
        async move {
            let th = i / per;
            r.getattr(d[th]).await.unwrap();
        }
    })
    .await;
    // PR 13f: the kernel's ctime SETATTR echo — once per rename / unlink on
    // a real mount — in ITS shape: no mtime, a monotone ctime, so every op
    // takes `setattr_locked`'s ABSORB arm (a parked refinement, zero
    // journal entries) and the phase prices the routed `setattr` box the
    // echo mints per op, not a `utimes` commit (the FUSE handler's own
    // future is the mount row's). The absorb count is asserted below.
    let echo0 = META_KV_TIMES_ECHO_ABSORBED.load(Ordering::Relaxed);
    // Ahead of the records' ctime (now), as the kernel's echo is: each op
    // parks a refinement, the realistic absorb.
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        + 1_000_000_000;
    let r = Arc::clone(&routed);
    let d = Arc::clone(&dirs);
    let setattr_us = phase("setattr(echo)", n, t, move |i| {
        let r = Arc::clone(&r);
        let d = Arc::clone(&d);
        async move {
            let th = i / per;
            r.setattr(
                d[th],
                None,
                None,
                None,
                None,
                None,
                None,
                Some(base + i as u64),
            )
            .await
            .unwrap();
        }
    })
    .await;
    assert_eq!(
        META_KV_TIMES_ECHO_ABSORBED.load(Ordering::Relaxed) - echo0,
        n as u64,
        "the setattr(echo) phase must run the echo's shape — every op absorbed"
    );
    let r = Arc::clone(&routed);
    let d = Arc::clone(&dirs);
    let rename_us = phase("rename", n, t, move |i| {
        let r = Arc::clone(&r);
        let d = Arc::clone(&d);
        async move {
            let th = i / per;
            r.rename(d[th], &format!("f{i}"), d[th], &format!("g{i}"), 0)
                .await
                .unwrap();
        }
    })
    .await;
    let r = Arc::clone(&routed);
    let d = Arc::clone(&dirs);
    let unlink_us = phase("unlink", n, t, move |i| {
        let r = Arc::clone(&r);
        let d = Arc::clone(&d);
        async move {
            let th = i / per;
            r.unlink(d[th], &format!("g{i}")).await.unwrap();
        }
    })
    .await;
    println!(
        "MICROBENCH SUMMARY µs/op: create {create_us:.2} mkdir {mkdir_us:.2} lookup+ {lookup_us:.2} \
         lookup- {neg_us:.2} getattr {getattr_us:.2} setattr {setattr_us:.2} rename {rename_us:.2} \
         unlink {unlink_us:.2}"
    );
    for v in &routed.volumes {
        v.shutdown().await.unwrap();
    }
}
