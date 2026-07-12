//! P1-5: background task admission control.

use squeezefs::bg_admit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_spawn_bg_rejects_when_saturated() {
    let cap = bg_admit::capacity();
    assert!(cap >= 32);

    // Hold all permits so spawn_bg must reject.
    let mut held = Vec::new();
    for _ in 0..cap {
        let p = bg_admit::BG_TASK_SEM
            .clone()
            .try_acquire_owned()
            .expect("should acquire up to capacity");
        held.push(p);
    }
    assert_eq!(bg_admit::available_permits(), 0);

    let ran = Arc::new(AtomicUsize::new(0));
    let ran2 = ran.clone();
    bg_admit::spawn_bg(async move {
        ran2.fetch_add(1, Ordering::SeqCst);
    });
    // Give the runtime a moment; rejected tasks never run.
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "saturated admission must drop best-effort work"
    );

    drop(held);
    assert!(bg_admit::available_permits() > 0);

    let ran3 = Arc::new(AtomicUsize::new(0));
    let r = ran3.clone();
    bg_admit::spawn_bg(async move {
        r.fetch_add(1, Ordering::SeqCst);
    });
    for _ in 0..50 {
        if ran3.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        ran3.load(Ordering::SeqCst),
        1,
        "after release, spawn_bg must run"
    );
}

#[test]
fn test_striped_read_concurrency_constant() {
    assert_eq!(bg_admit::STRIPED_READ_CONCURRENCY, 16);
}

/// P2-6: auto policy is cores-based and clamped; override is sticky until reset.
#[test]
fn test_striped_block_concurrency_policy_and_override() {
    // Ensure auto mode for this test (other tests may have set override).
    bg_admit::set_striped_block_concurrency(0);
    let auto = bg_admit::striped_block_concurrency();
    assert!(
        (4..=64).contains(&auto),
        "auto concurrency must be in [4, 64], got {auto}"
    );
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    assert_eq!(auto, cores.saturating_mul(2).clamp(4, 64));

    bg_admit::set_striped_block_concurrency(12);
    assert_eq!(bg_admit::striped_block_concurrency(), 12);
    bg_admit::set_striped_block_concurrency(1);
    assert_eq!(bg_admit::striped_block_concurrency(), 1);

    // Restore auto for other tests / process state.
    bg_admit::set_striped_block_concurrency(0);
    assert_eq!(bg_admit::striped_block_concurrency(), auto);
}
